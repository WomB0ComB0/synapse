//! Tenant-scoped, server-owned registry for outbound connector tools.

use sqlx::types::Json;

use crate::db::tenant_tx;
use crate::domain::{ApprovalMode, ToolDefinition, ToolRegisterRequest};
use crate::error::{Error, Result};
use crate::mcp::ConnectorImpl;

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;

type ToolRow = (
    String,
    String,
    Json<serde_json::Value>,
    Vec<String>,
    String,
    Option<String>,
    bool,
    i64,
);
type RuntimePolicyRow = (
    Json<serde_json::Value>,
    Vec<String>,
    String,
    Option<String>,
    bool,
    i64,
);

/// Immutable execution policy loaded from the current registry row.
pub struct RuntimeToolPolicy {
    pub revision: i64,
    pub requires_approval: bool,
    pub rollback_tool_id: Option<String>,
}

fn valid_tool_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

fn contains_ref(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.contains_key("$ref") || map.values().any(contains_ref)
        }
        serde_json::Value::Array(values) => values.iter().any(contains_ref),
        _ => false,
    }
}

fn validate_schema(schema: &serde_json::Value) -> Result<()> {
    if !schema.is_object() {
        return Err(Error::BadRequest(
            "input_schema must be a JSON object".into(),
        ));
    }
    if serde_json::to_vec(schema)
        .map_err(|e| Error::Internal(e.into()))?
        .len()
        > MAX_SCHEMA_BYTES
    {
        return Err(Error::BadRequest(format!(
            "input_schema exceeds {MAX_SCHEMA_BYTES} bytes"
        )));
    }
    // Runtime schemas are tenant data. Disallow every reference so validator construction can
    // never resolve an operator-controlled network or filesystem URI.
    if contains_ref(schema) {
        return Err(Error::BadRequest(
            "input_schema must be self-contained; $ref is not allowed".into(),
        ));
    }
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|e| Error::BadRequest(format!("invalid input_schema: {e}")))
}

fn validate_arguments(schema: &serde_json::Value, arguments: &serde_json::Value) -> Result<()> {
    if contains_ref(schema) {
        return Err(Error::Internal(anyhow::anyhow!(
            "stored tool schema contains forbidden $ref"
        )));
    }
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| Error::Internal(anyhow::anyhow!("invalid stored tool schema: {e}")))?;
    if validator.is_valid(arguments) {
        Ok(())
    } else {
        Err(Error::BadRequest(
            "tool arguments do not match the registered input_schema".into(),
        ))
    }
}

fn normalize_scopes(scopes: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(scopes.len());
    for raw in scopes {
        let scope = raw.trim();
        if scope.is_empty()
            || scope.len() > 255
            || !scope
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-' | b'/'))
        {
            return Err(Error::BadRequest(format!(
                "invalid connector scope {scope:?}"
            )));
        }
        normalized.push(scope.to_string());
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

fn approval_name(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::None => "none",
        ApprovalMode::Required => "required",
    }
}

fn row_to_definition(row: ToolRow) -> Result<ToolDefinition> {
    let approval_mode = match row.4.as_str() {
        "none" => ApprovalMode::None,
        "required" => ApprovalMode::Required,
        other => {
            return Err(Error::Internal(anyhow::anyhow!(
                "invalid stored tool approval_mode {other:?}"
            )))
        }
    };
    Ok(ToolDefinition {
        tool_id: row.0,
        description: row.1,
        input_schema: row.2 .0,
        required_scopes: row.3,
        approval_mode,
        rollback_tool_id: row.5,
        enabled: row.6,
        revision: row.7,
    })
}

/// Create or update one tenant-owned tool policy. Every update increments the revision.
pub async fn register(
    db: &sqlx::PgPool,
    tenant: &str,
    req: &ToolRegisterRequest,
) -> Result<ToolDefinition> {
    let tool_id = req.tool_id.trim();
    if !valid_tool_id(tool_id) {
        return Err(Error::BadRequest(
            "tool_id must be 1-255 ASCII letters, digits, dot, underscore, colon, or hyphen".into(),
        ));
    }
    if req.description.len() > MAX_DESCRIPTION_BYTES {
        return Err(Error::BadRequest(format!(
            "description exceeds {MAX_DESCRIPTION_BYTES} bytes"
        )));
    }
    validate_schema(&req.input_schema)?;
    let required_scopes = normalize_scopes(&req.required_scopes)?;
    let rollback_tool_id = req
        .rollback_tool_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if rollback_tool_id.is_some_and(|value| !valid_tool_id(value) || value == tool_id) {
        return Err(Error::BadRequest(
            "rollback_tool_id must be a different valid tool id".into(),
        ));
    }

    let mut tx = tenant_tx(db, tenant).await?;
    let row: ToolRow = sqlx::query_as(
        "INSERT INTO tool_definitions \
             (tenant_id, tool_id, description, input_schema, required_scopes, approval_mode, rollback_tool_id, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (tenant_id, tool_id) DO UPDATE SET \
             description = EXCLUDED.description, input_schema = EXCLUDED.input_schema, \
             required_scopes = EXCLUDED.required_scopes, approval_mode = EXCLUDED.approval_mode, \
             rollback_tool_id = EXCLUDED.rollback_tool_id, enabled = EXCLUDED.enabled, \
             revision = tool_definitions.revision + 1, updated_at = now() \
         RETURNING tool_id, description, input_schema, required_scopes, approval_mode, \
                   rollback_tool_id, enabled, revision",
    )
    .bind(tenant)
    .bind(tool_id)
    .bind(req.description.trim())
    .bind(Json(&req.input_schema))
    .bind(&required_scopes)
    .bind(approval_name(req.approval_mode))
    .bind(rollback_tool_id)
    .bind(req.enabled)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| Error::db_or_conflict(e, "tool definition conflicts with existing state"))?;
    tx.commit().await?;
    row_to_definition(row)
}

/// List the tenant's tool policies in deterministic id order.
pub async fn list(db: &sqlx::PgPool, tenant: &str) -> Result<Vec<ToolDefinition>> {
    let mut tx = tenant_tx(db, tenant).await?;
    let rows: Vec<ToolRow> = sqlx::query_as(
        "SELECT tool_id, description, input_schema, required_scopes, approval_mode, \
                rollback_tool_id, enabled, revision \
         FROM tool_definitions WHERE tenant_id = $1 ORDER BY tool_id",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    rows.into_iter().map(row_to_definition).collect()
}

/// Load and enforce the current policy for a real outbound connector call.
pub async fn govern_external_call(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: &str,
    tool_id: &str,
    arguments: &serde_json::Value,
    connector: &ConnectorImpl,
) -> Result<RuntimeToolPolicy> {
    let row: Option<RuntimePolicyRow> = sqlx::query_as(
        "SELECT input_schema, required_scopes, approval_mode, rollback_tool_id, enabled, revision \
         FROM tool_definitions WHERE tenant_id = $1 AND tool_id = $2",
    )
    .bind(tenant)
    .bind(tool_id)
    .fetch_optional(&mut **tx)
    .await?;
    let (schema, scopes, approval_mode, rollback_tool_id, enabled, revision) =
        row.ok_or(Error::Forbidden)?;
    if !enabled || !connector.supports_scopes(&scopes) {
        return Err(Error::Forbidden);
    }
    validate_arguments(&schema.0, arguments)?;
    Ok(RuntimeToolPolicy {
        revision,
        requires_approval: approval_mode == "required",
        rollback_tool_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_external_or_local_schema_refs() {
        let external = serde_json::json!({ "$ref": "https://attacker.example/schema.json" });
        let local =
            serde_json::json!({ "$ref": "#/$defs/x", "$defs": { "x": { "type": "object" } } });
        assert!(validate_schema(&external).is_err());
        assert!(validate_schema(&local).is_err());
    }

    #[test]
    fn validates_registered_arguments() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": { "message": { "type": "string", "maxLength": 20 } },
            "additionalProperties": false
        });
        validate_schema(&schema).unwrap();
        assert!(validate_arguments(&schema, &serde_json::json!({ "message": "ok" })).is_ok());
        assert!(validate_arguments(&schema, &serde_json::json!({ "message": 42 })).is_err());
        assert!(validate_arguments(&schema, &serde_json::json!({ "other": true })).is_err());
    }
}
