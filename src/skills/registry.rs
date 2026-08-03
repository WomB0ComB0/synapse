//! Versioned registry of governed skills.

use semver::Version;
use uuid::Uuid;

use crate::db::tenant_tx;
use crate::domain::{Skill, SkillGetResponse};
use crate::error::{Error, Result};

/// Stores and versions [`Skill`]s in Postgres, tenant-isolated by Row-Level
/// Security (migration `0009`).
///
/// Each `(tenant, skill_id, version)` is **immutable** once registered — new
/// behavior means a new version. `is_latest` tracks the highest *semantic*
/// version per `(tenant, skill_id)`; the partial unique index `uq_skills_latest`
/// guarantees at most one latest row.
pub struct SkillRegistry {
    db: sqlx::PgPool,
}

/// Columns selected for a full skill read (order matches [`SkillRow`]).
const SKILL_COLS: &str = "id, skill_id, version, name, summary, owners, triggers, \
     input_schema, output_schema, required_tools, policy_tags, examples, is_latest";

/// One `skills` row as stored (manifest + registry metadata), mapped from Postgres.
#[derive(sqlx::FromRow)]
struct SkillRow {
    id: Uuid,
    skill_id: String,
    version: String,
    name: String,
    summary: Option<String>,
    owners: Vec<String>,
    triggers: Vec<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    required_tools: Vec<String>,
    policy_tags: Vec<String>,
    examples: serde_json::Value,
    is_latest: bool,
}

impl From<SkillRow> for SkillGetResponse {
    fn from(r: SkillRow) -> Self {
        SkillGetResponse {
            id: r.id.to_string(),
            is_latest: r.is_latest,
            skill: Skill {
                skill_id: r.skill_id,
                version: r.version,
                name: r.name,
                summary: r.summary,
                owners: r.owners,
                triggers: r.triggers,
                input_schema: Some(r.input_schema),
                output_schema: Some(r.output_schema),
                required_tools: r.required_tools,
                policy_tags: r.policy_tags,
                examples: r.examples.as_array().cloned().unwrap_or_default(),
            },
        }
    }
}

impl SkillRegistry {
    /// Construct a registry over the given pool.
    pub fn new(db: sqlx::PgPool) -> Self {
        Self { db }
    }

    /// Register (version) a skill for `tenant`. Returns whether this version
    /// became the current latest.
    ///
    /// Errors: [`Error::BadRequest`] for empty ids or a non-semver version;
    /// [`Error::Conflict`] if that exact version already exists (immutable).
    pub async fn register(&self, tenant: &str, skill: &Skill) -> Result<bool> {
        let skill_id = skill.skill_id.trim();
        let version = skill.version.trim();
        if skill_id.is_empty() {
            return Err(Error::BadRequest("skill_id is required".into()));
        }
        if skill.name.trim().is_empty() {
            return Err(Error::BadRequest("name is required".into()));
        }
        let new_version = Version::parse(version).map_err(|e| {
            Error::BadRequest(format!("version '{version}' is not valid semver: {e}"))
        })?;
        // Build metadata carries no semver precedence, so `1.0.0` and `1.0.0+b1`
        // compare Equal yet would be distinct immutable rows — reject it so
        // uniqueness and latest-selection stay coherent. Prerelease (`1.0.0-rc1`)
        // is fine: it orders correctly and is meaningful.
        if !new_version.build.is_empty() {
            return Err(Error::BadRequest(format!(
                "version '{version}' carries build metadata (+{}); omit it (no registry precedence)",
                new_version.build
            )));
        }

        let mut tx = tenant_tx(&self.db, tenant).await?;

        // Serialize concurrent registers for this (tenant, skill_id). The
        // read-current-latest -> demote -> insert sequence below is a
        // read-modify-write; under READ COMMITTED a slow register of a LOWER
        // version could otherwise demote a HIGHER version that committed in
        // between and leave the wrong version flagged latest. A transaction-scoped
        // advisory lock (auto-released on commit/rollback), keyed per skill so
        // unrelated skills never contend, makes the sequence atomic. The key is
        // namespaced ("skills:...") so it can't collide with another subsystem's
        // advisory locks in the shared, database-global 64-bit key space.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("skills:{tenant}:{skill_id}"))
            .execute(&mut *tx)
            .await?;

        // Immutability: (tenant, skill_id, version) is write-once. The explicit
        // tenant_id predicates below are defense-in-depth beyond RLS.
        let existing: Option<(String,)> = sqlx::query_as(
            "SELECT version FROM skills WHERE tenant_id = $1 AND skill_id = $2 AND version = $3",
        )
        .bind(tenant)
        .bind(skill_id)
        .bind(version)
        .fetch_optional(&mut *tx)
        .await?;
        if existing.is_some() {
            return Err(Error::Conflict(format!(
                "skill '{skill_id}' version '{version}' already registered (versions are immutable)"
            )));
        }

        // Latest = highest semver. Compare against the current latest, and only
        // then flip the flag — demoting the old latest BEFORE inserting the new
        // one keeps the `uq_skills_latest` partial unique index satisfied.
        let current_latest: Option<(String,)> = sqlx::query_as(
            "SELECT version FROM skills WHERE tenant_id = $1 AND skill_id = $2 AND is_latest",
        )
        .bind(tenant)
        .bind(skill_id)
        .fetch_optional(&mut *tx)
        .await?;
        let becomes_latest = match &current_latest {
            None => true,
            Some((cur,)) => match Version::parse(cur) {
                Ok(cur_v) => new_version > cur_v,
                // A stored latest that isn't valid semver means data corruption or
                // a validation bypass — surface it for operators, but don't wedge
                // the registry: let the new (validated) version take over as latest.
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        stored_version = %cur,
                        skill_id = %skill_id,
                        "stored latest skill version is not valid semver; new version takes over"
                    );
                    true
                }
            },
        };
        if becomes_latest {
            sqlx::query(
                "UPDATE skills SET is_latest = false \
                 WHERE tenant_id = $1 AND skill_id = $2 AND is_latest",
            )
            .bind(tenant)
            .bind(skill_id)
            .execute(&mut *tx)
            .await?;
        }

        // Canonicalize tool ids + policy tags so the tool gateway's approval lookup
        // (exact array membership + the `human_approval_required` tag) cannot be
        // fooled OPEN by stray whitespace or tag casing. Tool ids are exact
        // identifiers (trimmed); policy tags are lowercased conventions.
        let required_tools: Vec<String> = skill
            .required_tools
            .iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();
        let policy_tags: Vec<String> = skill
            .policy_tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();

        let empty_obj = serde_json::json!({});
        sqlx::query(
            r#"
            INSERT INTO skills
                (tenant_id, skill_id, version, name, summary, owners, triggers,
                 input_schema, output_schema, required_tools, policy_tags, examples, is_latest)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
            "#,
        )
        .bind(tenant)
        .bind(skill_id)
        .bind(version)
        .bind(skill.name.trim())
        .bind(&skill.summary)
        .bind(&skill.owners)
        .bind(&skill.triggers)
        .bind(
            skill
                .input_schema
                .clone()
                .unwrap_or_else(|| empty_obj.clone()),
        )
        .bind(
            skill
                .output_schema
                .clone()
                .unwrap_or_else(|| empty_obj.clone()),
        )
        .bind(&required_tools)
        .bind(&policy_tags)
        .bind(serde_json::Value::Array(skill.examples.clone()))
        .bind(becomes_latest)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            Error::db_or_conflict(e, "skill version already registered (concurrent write)")
        })?;

        tx.commit().await?;
        Ok(becomes_latest)
    }

    /// Fetch one skill for `tenant`: a specific `version`, or the current latest
    /// when `version` is `None`. Returns `None` if no matching skill exists.
    pub async fn get(
        &self,
        tenant: &str,
        skill_id: &str,
        version: Option<&str>,
    ) -> Result<Option<SkillGetResponse>> {
        let skill_id = skill_id.trim();
        let sql = match version {
            Some(_) => format!(
                "SELECT {SKILL_COLS} FROM skills \
                 WHERE tenant_id = $1 AND skill_id = $2 AND version = $3"
            ),
            None => format!(
                "SELECT {SKILL_COLS} FROM skills \
                 WHERE tenant_id = $1 AND skill_id = $2 AND is_latest"
            ),
        };
        let mut q = sqlx::query_as::<_, SkillRow>(&sql)
            .bind(tenant)
            .bind(skill_id);
        if let Some(v) = version {
            q = q.bind(v.trim());
        }

        let mut tx = tenant_tx(&self.db, tenant).await?;
        let row = q.fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        Ok(row.map(Into::into))
    }
}
