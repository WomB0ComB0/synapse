//! Domain (de)serialization round-trip + defaulting tests. DB-free.

use synapse::domain::{
    ApprovalMode, Context, DataClassification, DocumentIngestRequest, RetrievalMode,
    RetrieveRequest, Skill, ToolExecuteRequest,
};

#[test]
fn skill_full_roundtrip() {
    let json = serde_json::json!({
        "skill_id": "skill.summarize",
        "version": "1.0.0",
        "name": "Summarize",
        "summary": "Summarize a document",
        "owners": ["team-brain"],
        "triggers": ["summarize"],
        "input_schema": { "type": "object" },
        "output_schema": { "type": "object" },
        "required_tools": [],
        "policy_tags": ["pii-safe"],
        "examples": []
    });

    let skill: Skill = serde_json::from_value(json).unwrap();
    assert_eq!(skill.skill_id, "skill.summarize");
    assert_eq!(skill.owners, vec!["team-brain".to_string()]);

    let back = serde_json::to_value(&skill).unwrap();
    assert_eq!(back["version"], "1.0.0");
    assert_eq!(back["policy_tags"][0], "pii-safe");
}

#[test]
fn skill_minimal_required_only() {
    let json = serde_json::json!({ "skill_id": "s", "version": "1", "name": "n" });
    let skill: Skill = serde_json::from_value(json).unwrap();
    assert!(skill.summary.is_none());
    assert!(skill.owners.is_empty());
    assert!(skill.input_schema.is_none());
}

#[test]
fn retrieve_request_applies_defaults() {
    let json = serde_json::json!({ "tenant_id": "t", "principal_id": "p", "query": "q" });
    let req: RetrieveRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.retrieval.mode, RetrievalMode::Hybrid);
    assert_eq!(req.retrieval.top_k, 10);
    assert!(!req.retrieval.rerank);
    assert!(!req.retrieval.include_graph);
    assert!(req.scope.team_ids.is_empty());
}

#[test]
fn retrieve_mode_parses_variants() {
    let vector: RetrieveRequest = serde_json::from_value(serde_json::json!({
        "tenant_id": "t", "principal_id": "p", "query": "q",
        "retrieval": { "mode": "vector", "top_k": 5 }
    }))
    .unwrap();
    assert_eq!(vector.retrieval.mode, RetrievalMode::Vector);
    assert_eq!(vector.retrieval.top_k, 5);
}

#[test]
fn context_roundtrip() {
    let ctx = Context {
        principal_id: "p1".into(),
        tenant_id: Some("t1".into()),
        team_ids: vec!["team-a".into()],
        role: Some("engineer".into()),
        location: None,
        approval_limit_usd: Some(500.0),
        preferred_tools: vec![],
        active_projects: vec![],
        policy_overrides: vec![],
        data_classification: DataClassification {
            contains_pii: true,
            special_category: false,
        },
        updated_at: None,
    };

    let s = serde_json::to_string(&ctx).unwrap();
    let back: Context = serde_json::from_str(&s).unwrap();
    assert_eq!(back.principal_id, "p1");
    assert_eq!(back.approval_limit_usd, Some(500.0));
    assert!(back.data_classification.contains_pii);
}

#[test]
fn tool_execute_defaults() {
    let json = serde_json::json!({
        "tenant_id": "t", "principal_id": "p", "tool_id": "jira.create_issue"
    });
    let req: ToolExecuteRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.policy.approval_mode, ApprovalMode::None);
    assert!(req.arguments.is_null());
}

#[test]
fn document_ingest_flattens_document_and_content() {
    let json = serde_json::json!({
        "doc_id": "d1",
        "tenant_id": "t1",
        "title": "Runbook",
        "content": "body text"
    });
    let req: DocumentIngestRequest = serde_json::from_value(json).unwrap();
    assert_eq!(req.document.doc_id, "d1");
    assert_eq!(req.document.tenant_id, "t1");
    assert_eq!(req.document.title.as_deref(), Some("Runbook"));
    assert_eq!(req.content.as_deref(), Some("body text"));
    // Defaulted collections/objects.
    assert!(req.document.team_scope.is_empty());
    assert!(!req.document.acl.inherit_from_source);
}
