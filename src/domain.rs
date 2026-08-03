//! Domain types + request/response DTOs.
//!
//! These structs mirror the **canonical JSON schemas** from the design report.
//! They drive the domain model, the DB tables (owned by migrations), the
//! OpenAPI surface, and the `schemas/*.json` files. Optional fields default so
//! callers can send minimal payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Skill
// ---------------------------------------------------------------------------

/// A versioned, governed capability an agent can invoke.
///
/// Required: `skill_id`, `version`, `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub skill_id: String,
    pub version: String,
    pub name: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub triggers: Vec<String>,
    /// JSON Schema describing accepted input.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// JSON Schema describing produced output.
    #[serde(default)]
    pub output_schema: Option<Value>,
    #[serde(default)]
    pub required_tools: Vec<String>,
    #[serde(default)]
    pub policy_tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Context / Principal profile
// ---------------------------------------------------------------------------

/// Data-classification flags on a principal's context (drives PII handling).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DataClassification {
    #[serde(default)]
    pub contains_pii: bool,
    #[serde(default)]
    pub special_category: bool,
}

/// A principal's context profile (personal/team/org).
///
/// Kept separate from org knowledge; PII is minimized and flagged via
/// [`DataClassification`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub principal_id: String,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub team_ids: Vec<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub approval_limit_usd: Option<f64>,
    #[serde(default)]
    pub preferred_tools: Vec<String>,
    #[serde(default)]
    pub active_projects: Vec<String>,
    /// Policy override identifiers (stored as `text[]`, matching migration 0003).
    #[serde(default)]
    pub policy_overrides: Vec<String>,
    #[serde(default)]
    pub data_classification: DataClassification,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Document + Chunk (canonical source-of-truth + derived unit)
// ---------------------------------------------------------------------------

/// Access-control list attached to a document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Acl {
    #[serde(default)]
    pub users: Vec<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub inherit_from_source: bool,
}

/// A canonical document (source-of-truth metadata).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub doc_id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub team_scope: Vec<String>,
    #[serde(default)]
    pub source_system: Option<String>,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub acl: Acl,
    /// Free-form metadata (JSONB in Postgres).
    #[serde(default)]
    pub metadata: Value,
}

/// A derived, retrievable chunk of a document.
///
/// Derived from the canonical [`Document`] and rebuildable. Records the
/// embedding model + dimensions so the vector index can be regenerated
/// deterministically. The raw vector itself is represented elsewhere as
/// `Vec<f32>` (see [`crate::retrieval::embed`]); here we keep only a reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub chunk_id: String,
    pub doc_id: String,
    pub tenant_id: String,
    pub ordinal: i64,
    #[serde(default)]
    pub section_path: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub token_count: Option<i64>,
    #[serde(default)]
    pub char_start: Option<i64>,
    #[serde(default)]
    pub char_end: Option<i64>,
    #[serde(default)]
    pub embedding_model: Option<String>,
    #[serde(default)]
    pub embedding_dimensions: Option<i64>,
    #[serde(default)]
    pub vector_ref: Option<String>,
    #[serde(default)]
    pub sparse_terms_ref: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

// ---------------------------------------------------------------------------
// Run (durable orchestration)
// ---------------------------------------------------------------------------

/// A durable workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub run_id: String,
    pub tenant_id: String,
    pub run_type: String,
    #[serde(default)]
    pub workflow_id: Option<String>,
    pub status: String,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// An append-only audit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub tenant_id: String,
    #[serde(default)]
    pub principal_id: Option<String>,
    pub action: String,
    pub resource: String,
    pub outcome: String,
    pub ts: DateTime<Utc>,
    #[serde(default)]
    pub metadata: Value,
}

// ===========================================================================
// Request / response DTOs (one pair per endpoint)
// ===========================================================================

// ---- POST /skills.register ----

/// Request body for `POST /skills.register` — the skill to register/version.
pub type SkillRegisterRequest = Skill;

/// Response for `POST /skills.register`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegisterResponse {
    pub skill_id: String,
    pub version: String,
    pub status: String,
}

// ---- POST /skills.get ----

/// Request body for `POST /skills.get` — fetch one versioned skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillGetRequest {
    pub skill_id: String,
    /// Specific version to fetch; when omitted, the current latest is returned.
    #[serde(default)]
    pub version: Option<String>,
}

/// Response for `POST /skills.get` — the stored manifest plus registry metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillGetResponse {
    /// Stable surrogate id of this exact skill version.
    pub id: String,
    /// Whether this is the current latest version for its `skill_id`.
    pub is_latest: bool,
    #[serde(flatten)]
    pub skill: Skill,
}

// ---- POST /documents.ingest ----

/// Request body for `POST /documents.ingest`.
///
/// The canonical [`Document`] metadata is flattened at the top level; an
/// optional `content` field carries raw text to chunk + embed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIngestRequest {
    #[serde(flatten)]
    pub document: Document,
    #[serde(default)]
    pub content: Option<String>,
}

/// Response for `POST /documents.ingest`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIngestResponse {
    pub doc_id: String,
    pub status: String,
    pub chunks_ingested: usize,
}

/// Request body for `POST /documents.reembed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentReembedRequest {
    pub doc_id: String,
}

/// Response for `POST /documents.reembed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentReembedResponse {
    pub doc_id: String,
    pub status: String,
    pub chunks_queued: usize,
}

// ---- POST /context.upsert ----

/// Request body for `POST /context.upsert` — the principal context to upsert.
pub type ContextUpsertRequest = Context;

/// Response for `POST /context.upsert`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextUpsertResponse {
    pub principal_id: String,
    pub status: String,
}

// ---- POST /context.get ----

/// Request body for `POST /context.get` — fetch a principal's context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextGetRequest {
    pub principal_id: String,
}

// ---- POST /retrieve ----

/// Retrieval mode selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetrievalMode {
    #[default]
    Hybrid,
    Vector,
    Lexical,
}

fn default_mode() -> RetrievalMode {
    RetrievalMode::Hybrid
}

fn default_top_k() -> u32 {
    10
}

/// Scope filter for retrieval (team/project fences layered on top of ACLs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RetrieveScope {
    #[serde(default)]
    pub team_ids: Vec<String>,
    #[serde(default)]
    pub project_ids: Vec<String>,
}

/// Retrieval tuning options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalOptions {
    #[serde(default = "default_mode")]
    pub mode: RetrievalMode,
    #[serde(default = "default_top_k")]
    pub top_k: u32,
    #[serde(default)]
    pub rerank: bool,
    #[serde(default)]
    pub include_graph: bool,
    /// Opt-in MMR (Maximal-Marginal-Relevance) diversity re-ranking of the final `top_k`. When
    /// **true**, the over-fetched candidate pool is re-ranked to trade query-relevance against
    /// dissimilarity to already-selected results (so near-duplicate chunks don't crowd out the
    /// top_k). Off (default) leaves the pure relevance ordering unchanged.
    #[serde(default)]
    pub mmr: bool,
    /// Per-request override for the MMR relevance↔diversity balance `λ` (`1.0` = pure relevance =
    /// no diversification; `0.0` = pure diversity). Clamped to `[0, 1]`. When absent, the server's
    /// configured default ([`crate::config::Config::retrieval_mmr_lambda`]) is used. Only meaningful
    /// when `mmr` is `true`.
    #[serde(default)]
    pub mmr_lambda: Option<f64>,
}

impl Default for RetrievalOptions {
    fn default() -> Self {
        Self {
            mode: RetrievalMode::Hybrid,
            top_k: default_top_k(),
            rerank: false,
            include_graph: false,
            mmr: false,
            mmr_lambda: None,
        }
    }
}

/// Request body for `POST /retrieve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrieveRequest {
    pub tenant_id: String,
    pub principal_id: String,
    pub query: String,
    #[serde(default)]
    pub scope: RetrieveScope,
    #[serde(default)]
    pub retrieval: RetrievalOptions,
}

/// A single retrieval hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalResult {
    pub chunk_id: String,
    pub doc_id: String,
    pub score: f32,
    pub text: String,
    #[serde(default)]
    pub source_uri: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

/// Response for `POST /retrieve`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrieveResponse {
    pub results: Vec<RetrievalResult>,
    pub trace_id: String,
}

// ---- POST /tool.execute ----

/// Whether a tool call requires human approval before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    #[default]
    None,
    Required,
}

/// Policy block on a tool call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPolicy {
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Request body for `POST /tool.execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecuteRequest {
    pub tenant_id: String,
    pub principal_id: String,
    pub tool_id: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub policy: ToolPolicy,
}

/// Response for `POST /tool.execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecuteResponse {
    pub tool_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    pub status: String,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub requires_approval: bool,
}

// ---- server-owned tool registry + approval/rollback lifecycle ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub tool_id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_object_schema")]
    pub input_schema: Value,
    #[serde(default)]
    pub required_scopes: Vec<String>,
    #[serde(default = "default_required_approval")]
    pub approval_mode: ApprovalMode,
    #[serde(default)]
    pub rollback_tool_id: Option<String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub revision: i64,
}

fn default_object_schema() -> Value {
    serde_json::json!({ "type": "object" })
}

fn default_required_approval() -> ApprovalMode {
    ApprovalMode::Required
}

pub type ToolRegisterRequest = ToolDefinition;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRegisterResponse {
    pub tool: ToolDefinition,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolListResponse {
    pub tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDecisionRequest {
    pub execution_id: String,
    pub decision: ToolDecision,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRollbackRequest {
    pub execution_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLifecycleResponse {
    pub execution_id: String,
    pub tool_id: String,
    pub status: String,
    #[serde(default)]
    pub output: Value,
}

// ---- POST /runs.start + POST /runs.resume ----

/// Callback hooks for a durable run (human-in-the-loop + webhook).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunCallbacks {
    #[serde(default)]
    pub human_approval: bool,
    #[serde(default)]
    pub webhook: Option<String>,
}

/// Request body for `POST /runs.start`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunsStartRequest {
    pub tenant_id: String,
    pub run_type: String,
    #[serde(default)]
    pub workflow_id: Option<String>,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub callbacks: RunCallbacks,
}

/// Request body for `POST /runs.resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunsResumeRequest {
    pub run_id: String,
    pub token: String,
    #[serde(default)]
    pub resume_input: Value,
}

/// Response for both `runs.start` and `runs.resume`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResponse {
    pub run_id: String,
    pub status: String,
    /// Opaque token used to resume a suspended (e.g. awaiting-approval) run.
    #[serde(default)]
    pub resume_token: Option<String>,
}

// ---- GET /audit/events ----

/// Response for `GET /audit/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEventsResponse {
    pub events: Vec<AuditEvent>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// Query parameters for `GET /audit/events` (keyset pagination).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuditEventsQuery {
    /// Filter by action, e.g. `"skills.register"`.
    #[serde(default)]
    pub action: Option<String>,
    /// Filter by the acting principal.
    #[serde(default)]
    pub principal_id: Option<String>,
    /// Filter by resource.
    #[serde(default)]
    pub resource: Option<String>,
    /// Opaque cursor from a prior response's `next_cursor` (the last event_id).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Page size (clamped to `1..=200`; default 50).
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /teams.add_member` — add (or update the team-role of) a principal in a team.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamAddMemberRequest {
    pub team_id: String,
    pub principal_id: String,
    /// Optional role within the team (e.g. "member" | "lead").
    #[serde(default)]
    pub role: Option<String>,
}

/// `POST /teams.remove_member` — remove a principal from a team.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamRemoveMemberRequest {
    pub team_id: String,
    pub principal_id: String,
}

/// Response for the team-membership endpoints (`status` is "added" | "removed").
#[derive(Debug, Clone, Serialize)]
pub struct TeamMemberResponse {
    pub team_id: String,
    pub principal_id: String,
    pub status: String,
}

/// `POST /teams.create` — explicitly create a team (admin-only), owned by the caller.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamCreateRequest {
    pub team_id: String,
    /// Optional human-readable team name.
    #[serde(default)]
    pub name: Option<String>,
}

/// Response for `POST /teams.create`.
#[derive(Debug, Clone, Serialize)]
pub struct TeamCreateResponse {
    pub team_id: String,
    pub status: String,
}

/// One team in a `POST /teams.list` response.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TeamSummary {
    pub team_id: String,
    pub name: Option<String>,
    pub owners: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// Response for `POST /teams.list`.
#[derive(Debug, Clone, Serialize)]
pub struct TeamListResponse {
    pub teams: Vec<TeamSummary>,
}

/// `POST /teams.members` — list the members of a team (owner/admin only).
#[derive(Debug, Clone, Deserialize)]
pub struct TeamMembersRequest {
    pub team_id: String,
}

/// One member in a `POST /teams.members` response.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TeamMemberEntry {
    pub principal_id: String,
    pub role: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Response for `POST /teams.members`.
#[derive(Debug, Clone, Serialize)]
pub struct TeamMembersResponse {
    pub team_id: String,
    pub members: Vec<TeamMemberEntry>,
}

/// `POST /documents.grant` — grant a document ACL to a user or group. `grantee_id`
/// is the principal_id (for `grantee_type="user"`) or team_id (for `"group"`).
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentGrantRequest {
    pub doc_id: String,
    pub grantee_type: String,
    pub grantee_id: String,
    /// "read" | "write" | "admin" (default "read").
    #[serde(default)]
    pub permission: Option<String>,
}

/// `POST /documents.revoke` — revoke a document ACL. Omit `permission` to revoke
/// EVERY permission for the grantee on the document.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentRevokeRequest {
    pub doc_id: String,
    pub grantee_type: String,
    pub grantee_id: String,
    #[serde(default)]
    pub permission: Option<String>,
}

/// Response for the document-ACL grant/revoke endpoints (`status` is "granted" |
/// "revoked"; `permission` is the acted-on permission, or "*" for a revoke-all).
#[derive(Debug, Clone, Serialize)]
pub struct DocumentAclResponse {
    pub doc_id: String,
    pub grantee_type: String,
    pub grantee_id: String,
    pub permission: String,
    pub status: String,
}
