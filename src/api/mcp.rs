//! Inbound Model Context Protocol (MCP) server for coding agents.
//!
//! This is a stateless Streamable HTTP endpoint. It implements the stable
//! 2025-11-25 lifecycle and tool methods without protocol sessions; servers are
//! allowed to omit sessions when they do not need them. Every request uses the
//! normal [`Principal`] extractor, so REST and MCP share JWT verification,
//! revocation, rate limiting, policy checks, RLS, persistence, and auditing.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::api::{context, documents, retrieve, runs, skills, tools};
use crate::auth::Principal;
use crate::domain::{
    ContextGetRequest, ContextUpsertRequest, DocumentIngestRequest, DocumentReembedRequest,
    RetrieveRequest, RunsResumeRequest, RunsStartRequest, SkillGetRequest, SkillRegisterRequest,
    ToolDecisionRequest, ToolExecuteRequest, ToolRegisterRequest, ToolRollbackRequest,
};
use crate::error::Error;
use crate::state::AppState;

const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug)]
enum ToolFailure {
    InvalidArguments(String),
    Application(Error),
}

impl From<Error> for ToolFailure {
    fn from(value: Error) -> Self {
        Self::Application(value)
    }
}

/// Streamable HTTP GET is reserved for a server-to-client event stream. Synapse
/// is deliberately stateless and emits no unsolicited notifications, so it
/// advertises POST-only behavior with 405 instead of keeping idle SSE sessions.
pub async fn get(_principal: Principal) -> Response {
    let mut response = StatusCode::METHOD_NOT_ALLOWED.into_response();
    response
        .headers_mut()
        .insert(axum::http::header::ALLOW, HeaderValue::from_static("POST"));
    response
}

/// Handle one MCP JSON-RPC request or notification.
pub async fn post(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Browser-originated MCP requests are rejected by default. Native coding
    // agents do not send Origin; rejecting it closes DNS-rebinding attacks for
    // localhost deployments without maintaining a second origin allow-list.
    if headers.contains_key(axum::http::header::ORIGIN) {
        return rpc_http_error(
            StatusCode::FORBIDDEN,
            Value::Null,
            -32000,
            "browser Origin requests are not allowed",
        );
    }

    let request: RpcRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return rpc_http_error(StatusCode::BAD_REQUEST, Value::Null, -32700, "invalid JSON")
        }
    };
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc != "2.0" || request.method.trim().is_empty() {
        return rpc_http_error(
            StatusCode::BAD_REQUEST,
            id,
            -32600,
            "invalid JSON-RPC request",
        );
    }
    if let Err(message) = validate_transport_headers(&headers, &request) {
        return rpc_http_error(StatusCode::BAD_REQUEST, id, -32600, &message);
    }

    // Notifications never receive a JSON-RPC body.
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    let result = match request.method.as_str() {
        "initialize" => Ok(initialize_result(&request.params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&state, &principal, &request.params).await,
        _ => return rpc_http_error(StatusCode::OK, id, -32601, "method not found"),
    };

    match result {
        Ok(value) => rpc_result(id, value),
        Err(ToolFailure::InvalidArguments(message)) => {
            rpc_http_error(StatusCode::OK, id, -32602, &message)
        }
        Err(ToolFailure::Application(error)) => rpc_result(id, tool_error(error)),
    }
}

fn validate_transport_headers(headers: &HeaderMap, request: &RpcRequest) -> Result<(), String> {
    if request.method != "initialize" {
        if let Some(version) = header_text(headers, "mcp-protocol-version") {
            if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
                return Err(format!("unsupported MCP protocol version {version:?}"));
            }
        }
    }

    if let Some(method) = header_text(headers, "mcp-method") {
        if method != request.method {
            return Err("Mcp-Method header does not match the JSON-RPC method".to_string());
        }
    }
    if request.method == "tools/call" {
        if let Some(header_name) = header_text(headers, "mcp-name") {
            let body_name = request
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if header_name != body_name {
                return Err("Mcp-Name header does not match params.name".to_string());
            }
        }
    }
    Ok(())
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn initialize_result(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        LATEST_PROTOCOL_VERSION
    };

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": "synapse",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Use Synapse for governed organizational retrieval, durable context, versioned skills, and audited workflows."
    })
}

async fn call_tool(
    state: &AppState,
    principal: &Principal,
    params: &Value,
) -> Result<Value, ToolFailure> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolFailure::InvalidArguments("params.name is required".to_string()))?;
    let mut arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(ToolFailure::InvalidArguments(
            "params.arguments must be an object".to_string(),
        ));
    }

    let output = match name {
        "synapse_decide_tool" => {
            let request: ToolDecisionRequest = decode(arguments)?;
            let Json(response) =
                tools::decide(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_retrieve" => {
            inject_identity(&mut arguments, principal, true)?;
            let request: RetrieveRequest = decode(arguments)?;
            let Json(response) =
                retrieve::retrieve(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_ingest_document" => {
            inject_identity(&mut arguments, principal, false)?;
            let request: DocumentIngestRequest = decode(arguments)?;
            let Json(response) =
                documents::ingest(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_reembed_document" => {
            let request: DocumentReembedRequest = decode(arguments)?;
            let Json(response) =
                documents::reembed(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_register_tool" => {
            let request: ToolRegisterRequest = decode(arguments)?;
            let Json(response) =
                tools::register(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_list_tools" => {
            let Json(response) = tools::list(State(state.clone()), principal.clone()).await?;
            encode(response)?
        }
        "synapse_get_context" => {
            let request: ContextGetRequest = decode(arguments)?;
            let Json(response) =
                context::get(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_upsert_context" => {
            inject_identity(&mut arguments, principal, false)?;
            let request: ContextUpsertRequest = decode(arguments)?;
            let Json(response) =
                context::upsert(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_get_skill" => {
            let request: SkillGetRequest = decode(arguments)?;
            let Json(response) =
                skills::get(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_register_skill" => {
            let request: SkillRegisterRequest = decode(arguments)?;
            let Json(response) =
                skills::register(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_start_run" => {
            let request_headers = idempotency_headers(&mut arguments)?;
            inject_identity(&mut arguments, principal, false)?;
            let request: RunsStartRequest = decode(arguments)?;
            let Json(response) = runs::start(
                State(state.clone()),
                principal.clone(),
                request_headers,
                Json(request),
            )
            .await?;
            encode(response)?
        }
        "synapse_resume_run" => {
            let request: RunsResumeRequest = decode(arguments)?;
            let Json(response) =
                runs::resume(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        "synapse_execute_tool" => {
            let request_headers = idempotency_headers(&mut arguments)?;
            inject_identity(&mut arguments, principal, true)?;
            let request: ToolExecuteRequest = decode(arguments)?;
            let Json(response) = tools::execute(
                State(state.clone()),
                principal.clone(),
                request_headers,
                Json(request),
            )
            .await?;
            encode(response)?
        }
        "synapse_rollback_tool" => {
            let request: ToolRollbackRequest = decode(arguments)?;
            let Json(response) =
                tools::rollback(State(state.clone()), principal.clone(), Json(request)).await?;
            encode(response)?
        }
        _ => {
            return Err(ToolFailure::InvalidArguments(format!(
                "unknown tool {name:?}"
            )))
        }
    };

    Ok(tool_success(output))
}

fn inject_identity(
    arguments: &mut Value,
    principal: &Principal,
    include_principal: bool,
) -> Result<(), ToolFailure> {
    let tenant = principal.authenticated_tenant()?.to_string();
    let object = arguments.as_object_mut().ok_or_else(|| {
        ToolFailure::InvalidArguments("params.arguments must be an object".to_string())
    })?;
    object.insert("tenant_id".to_string(), Value::String(tenant));
    if include_principal {
        object.insert(
            "principal_id".to_string(),
            Value::String(principal.principal_id.clone()),
        );
    }
    Ok(())
}

fn idempotency_headers(arguments: &mut Value) -> Result<HeaderMap, ToolFailure> {
    let object = arguments.as_object_mut().ok_or_else(|| {
        ToolFailure::InvalidArguments("params.arguments must be an object".to_string())
    })?;
    let mut headers = HeaderMap::new();
    if let Some(value) = object.remove("idempotency_key") {
        let key = value.as_str().ok_or_else(|| {
            ToolFailure::InvalidArguments("idempotency_key must be a string".to_string())
        })?;
        let header = HeaderValue::from_str(key).map_err(|_| {
            ToolFailure::InvalidArguments("idempotency_key is not a valid header value".to_string())
        })?;
        headers.insert("idempotency-key", header);
    }
    Ok(headers)
}

fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ToolFailure> {
    serde_json::from_value(value)
        .map_err(|error| ToolFailure::InvalidArguments(format!("invalid tool arguments: {error}")))
}

fn encode<T: serde::Serialize>(value: T) -> Result<Value, ToolFailure> {
    serde_json::to_value(value).map_err(|error| {
        ToolFailure::Application(Error::Internal(anyhow::anyhow!(
            "failed to serialize MCP tool response: {error}"
        )))
    })
}

fn tool_success(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_error(error: Error) -> Value {
    let message = match &error {
        Error::BadRequest(_)
        | Error::NotFound(_)
        | Error::Conflict(_)
        | Error::Unauthorized
        | Error::Forbidden
        | Error::TooManyRequests { .. } => error.to_string(),
        Error::Upstream(_) => "an upstream dependency failed".to_string(),
        Error::Db(_) | Error::Internal(_) => "an internal service error occurred".to_string(),
    };
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn rpc_result(id: Value, result: Value) -> Response {
    (
        StatusCode::OK,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        })),
    )
        .into_response()
}

fn rpc_http_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message
            }
        })),
    )
        .into_response()
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool(
            "synapse_decide_tool",
            "Approve or deny a standalone pending tool execution. Requires an admin role.",
            schema(
                &["execution_id", "decision"],
                json!({
                    "execution_id": {"type": "string"},
                    "decision": {"type": "string", "enum": ["approve", "deny"]},
                    "reason": {"type": "string"}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_execute_tool",
            "Execute a policy-governed external tool. Use an idempotency key for side effects.",
            schema(
                &["tool_id"],
                json!({
                    "tool_id": {"type": "string"},
                    "arguments": {"type": "object"},
                    "policy": {"type": "object"},
                    "idempotency_key": {"type": "string"}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_get_context",
            "Read a governed principal context profile.",
            schema(&["principal_id"], json!({"principal_id": {"type": "string"}})),
            true,
            false,
        ),
        tool(
            "synapse_get_skill",
            "Read the latest or a specific version of a registered skill.",
            schema(
                &["skill_id"],
                json!({
                    "skill_id": {"type": "string"},
                    "version": {"type": "string"}
                }),
            ),
            true,
            false,
        ),
        tool(
            "synapse_ingest_document",
            "Ingest or replace a canonical document and its Gemini retrieval chunks.",
            schema(
                &["doc_id", "content"],
                json!({
                    "doc_id": {"type": "string"},
                    "content": {"type": "string"},
                    "team_scope": {"type": "array", "items": {"type": "string"}},
                    "source_system": {"type": "string"},
                    "source_uri": {"type": "string"},
                    "title": {"type": "string"},
                    "content_type": {"type": "string"},
                    "language": {"type": "string"},
                    "version": {"type": "string"},
                    "owners": {"type": "array", "items": {"type": "string"}},
                    "acl": {"type": "object"},
                    "metadata": {"type": "object"}
                }),
            ),
            false,
            false,
        ),
        tool(
            "synapse_list_tools",
            "List the current tenant-owned tool contracts and approval policies.",
            schema(&[], json!({})),
            true,
            false,
        ),
        tool(
            "synapse_reembed_document",
            "Rebuild an existing document with the configured embedding model while preserving canonical text and ACLs.",
            schema(&["doc_id"], json!({"doc_id": {"type": "string"}})),
            false,
            false,
        ),
        tool(
            "synapse_register_skill",
            "Register an immutable versioned skill manifest.",
            schema(
                &["skill_id", "version", "name"],
                json!({
                    "skill_id": {"type": "string"},
                    "version": {"type": "string"},
                    "name": {"type": "string"},
                    "summary": {"type": "string"},
                    "owners": {"type": "array", "items": {"type": "string"}},
                    "triggers": {"type": "array", "items": {"type": "string"}},
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "required_tools": {"type": "array", "items": {"type": "string"}},
                    "policy_tags": {"type": "array", "items": {"type": "string"}},
                    "examples": {"type": "array"}
                }),
            ),
            false,
            false,
        ),
        tool(
            "synapse_register_tool",
            "Create or update a tenant-owned tool contract. Requires an admin role.",
            schema(
                &["tool_id"],
                json!({
                    "tool_id": {"type": "string"},
                    "description": {"type": "string"},
                    "input_schema": {"type": "object"},
                    "required_scopes": {"type": "array", "items": {"type": "string"}},
                    "approval_mode": {"type": "string", "enum": ["none", "required"]},
                    "rollback_tool_id": {"type": "string"},
                    "enabled": {"type": "boolean"}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_resume_run",
            "Resume a durable workflow waiting for human input or approval.",
            schema(
                &["run_id", "token"],
                json!({
                    "run_id": {"type": "string"},
                    "token": {"type": "string"},
                    "resume_input": {}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_retrieve",
            "Retrieve permission-filtered organizational knowledge with hybrid vector and lexical search.",
            schema(
                &["query"],
                json!({
                    "query": {"type": "string"},
                    "scope": {"type": "object"},
                    "retrieval": {"type": "object"}
                }),
            ),
            true,
            false,
        ),
        tool(
            "synapse_rollback_tool",
            "Execute the registered compensation tool exactly once for a completed execution. Requires an admin role.",
            schema(
                &["execution_id"],
                json!({
                    "execution_id": {"type": "string"},
                    "reason": {"type": "string"}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_start_run",
            "Start an idempotent durable workflow run.",
            schema(
                &["run_type"],
                json!({
                    "run_type": {"type": "string"},
                    "workflow_id": {"type": "string"},
                    "input": {},
                    "callbacks": {"type": "object"},
                    "idempotency_key": {"type": "string"}
                }),
            ),
            false,
            true,
        ),
        tool(
            "synapse_upsert_context",
            "Create or update a governed principal context profile.",
            schema(
                &["principal_id"],
                json!({
                    "principal_id": {"type": "string"},
                    "team_ids": {"type": "array", "items": {"type": "string"}},
                    "role": {"type": "string"},
                    "location": {"type": "string"},
                    "approval_limit_usd": {"type": "number"},
                    "preferred_tools": {"type": "array", "items": {"type": "string"}},
                    "active_projects": {"type": "array", "items": {"type": "string"}},
                    "policy_overrides": {"type": "array", "items": {"type": "string"}},
                    "data_classification": {"type": "object"}
                }),
            ),
            false,
            false,
        ),
    ]
}

fn schema(required: &[&str], properties: Value) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn tool(
    name: &str,
    description: &str,
    input_schema: Value,
    read_only: bool,
    destructive: bool,
) -> Value {
    let mut annotations = Map::new();
    annotations.insert("readOnlyHint".to_string(), Value::Bool(read_only));
    annotations.insert("destructiveHint".to_string(), Value::Bool(destructive));
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
        "annotations": Value::Object(annotations)
    })
}
