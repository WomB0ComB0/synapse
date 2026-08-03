//! End-to-end governance for registered outbound tools, standalone approvals, and rollback.

mod common;

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use common::{app_pool, apply_schema, config_for, post_json, TestDb};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;

use synapse::state::AppState;

type MockCall = (Value, Option<String>);
type CallLog = Arc<Mutex<Vec<MockCall>>>;

#[derive(Clone, Default)]
struct MockState {
    calls: CallLog,
}

async fn tools_call(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    state
        .calls
        .lock()
        .unwrap()
        .push((body.clone(), authorization));
    Json(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "content": [{"type": "text", "text": "ok"}],
            "tool": body["params"]["name"],
            "arguments": body["params"]["arguments"],
            "isError": false
        }
    }))
}

async fn spawn_mock() -> (String, CallLog) {
    let state = MockState::default();
    let calls = state.calls.clone();
    let app = Router::new().route("/", post(tools_call)).with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), calls)
}

async fn register_tool(router: &Router, body: Value) -> Value {
    let (status, response) = post_json(router, "/tools.register", "tenant_a", "admin", body).await;
    assert_eq!(status, StatusCode::OK, "register tool: {response}");
    response
}

#[tokio::test]
async fn registry_approval_and_rollback_are_fail_closed_and_idempotent() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset - skipping DB-gated integration test");
        return;
    };

    let test_db = TestDb::create(&base_url, "toolgovernance").await;
    let admin_db = PgPoolOptions::new()
        .max_connections(4)
        .connect(&test_db.url)
        .await
        .expect("connect admin");
    apply_schema(&admin_db, &test_db.role).await;
    sqlx::raw_sql(
        "INSERT INTO principals (tenant_id, principal_id, role) VALUES
             ('tenant_a', 'admin', 'admin'),
             ('tenant_a', 'member', 'member'),
             ('tenant_b', 'admin', 'admin');",
    )
    .execute(&admin_db)
    .await
    .expect("seed authoritative roles");

    let (endpoint, calls) = spawn_mock().await;
    let mut config = config_for(&test_db.url);
    config.mcp_endpoint = Some(endpoint);
    config.mcp_auth_token = Some("connector-secret".into());
    config.mcp_scopes = vec!["deploy:write".into()];
    let router = synapse::app(AppState::new(
        app_pool(&test_db.url, &test_db.role).await,
        config,
    ));

    let execute = |tool_id: &str, arguments: Value| {
        json!({
            "tenant_id": "tenant_a",
            "principal_id": "member",
            "tool_id": tool_id,
            "arguments": arguments
        })
    };

    let (status, _) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "member",
        execute("unregistered", json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(calls.lock().unwrap().is_empty());

    let (status, _) = post_json(
        &router,
        "/tools.register",
        "tenant_a",
        "member",
        json!({"tool_id": "member-owned", "enabled": true}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "registry mutation is admin-only"
    );

    let (status, _) = post_json(
        &router,
        "/tools.register",
        "tenant_a",
        "admin",
        json!({
            "tool_id": "bad-schema",
            "input_schema": {"$ref": "https://attacker.example/schema.json"},
            "enabled": true
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "schema references fail closed"
    );

    register_tool(
        &router,
        json!({
            "tool_id": "echo.rollback",
            "description": "Compensate an echo call",
            "input_schema": {
                "type": "object",
                "required": [
                    "original_execution_id",
                    "original_tool_id",
                    "original_arguments",
                    "original_result"
                ],
                "properties": {
                    "original_execution_id": {"type": "string"},
                    "original_tool_id": {"type": "string"},
                    "original_arguments": {"type": "object"},
                    "original_result": {"type": "object"},
                    "reason": {"type": ["string", "null"]}
                },
                "additionalProperties": false
            },
            "required_scopes": ["deploy:write"],
            "approval_mode": "none",
            "enabled": true
        }),
    )
    .await;
    let registered = register_tool(
        &router,
        json!({
            "tool_id": "echo",
            "description": "Governed echo",
            "input_schema": {
                "type": "object",
                "required": ["msg"],
                "properties": {"msg": {"type": "string"}},
                "additionalProperties": false
            },
            "required_scopes": ["deploy:write"],
            "approval_mode": "required",
            "rollback_tool_id": "echo.rollback",
            "enabled": true
        }),
    )
    .await;
    assert_eq!(registered["tool"]["revision"], 1);

    let (status, listed) = post_json(&router, "/tools.list", "tenant_a", "member", json!({})).await;
    assert_eq!(status, StatusCode::OK, "members may inspect contracts");
    assert_eq!(listed["tools"].as_array().unwrap().len(), 2);

    let (status, _) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "member",
        execute("echo", json!({"unexpected": true})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "arguments must match the schema"
    );
    assert!(calls.lock().unwrap().is_empty());

    let (status, pending) = post_json(
        &router,
        "/tool.execute",
        "tenant_a",
        "member",
        execute("echo", json!({"msg": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "registered call creates an intent");
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["requires_approval"], true);
    let execution_id = pending["execution_id"].as_str().unwrap().to_string();
    assert!(calls.lock().unwrap().is_empty());

    let decision = json!({"execution_id": execution_id, "decision": "approve"});
    let (status, _) = post_json(
        &router,
        "/tools.decide",
        "tenant_a",
        "member",
        decision.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "approval is admin-only");
    assert!(calls.lock().unwrap().is_empty());

    let (status, approved) = post_json(
        &router,
        "/tools.decide",
        "tenant_a",
        "admin",
        decision.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin approval dispatches the tool");
    assert_eq!(approved["status"], "executed");
    assert_eq!(approved["output"]["arguments"]["msg"], "hello");
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0["params"]["name"], "echo");
        assert_eq!(calls[0].1.as_deref(), Some("Bearer connector-secret"));
    }

    let (status, replay) = post_json(&router, "/tools.decide", "tenant_a", "admin", decision).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["execution_id"], execution_id);
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "approval replay is side-effect free"
    );

    // Registry contracts remain mutable, but compensation for an executed side effect must not.
    // Change the original tool to point at a different valid handler after execution; rollback
    // must still use the handler snapshotted when the original call was dispatched.
    register_tool(
        &router,
        json!({
            "tool_id": "echo.rollback.changed",
            "description": "A later compensation contract",
            "input_schema": {
                "type": "object",
                "required": [
                    "original_execution_id",
                    "original_tool_id",
                    "original_arguments",
                    "original_result"
                ],
                "properties": {
                    "original_execution_id": {"type": "string"},
                    "original_tool_id": {"type": "string"},
                    "original_arguments": {"type": "object"},
                    "original_result": {"type": "object"},
                    "reason": {"type": ["string", "null"]}
                },
                "additionalProperties": false
            },
            "required_scopes": ["deploy:write"],
            "approval_mode": "none",
            "enabled": true
        }),
    )
    .await;
    let updated = register_tool(
        &router,
        json!({
            "tool_id": "echo",
            "description": "Governed echo with a new future compensation handler",
            "input_schema": {
                "type": "object",
                "required": ["msg"],
                "properties": {"msg": {"type": "string"}},
                "additionalProperties": false
            },
            "required_scopes": ["deploy:write"],
            "approval_mode": "required",
            "rollback_tool_id": "echo.rollback.changed",
            "enabled": true
        }),
    )
    .await;
    assert_eq!(updated["tool"]["revision"], 2);

    let (rollback_snapshot,): (Option<String>,) =
        sqlx::query_as("SELECT rollback_tool_id FROM tool_executions WHERE id = $1")
            .bind(uuid::Uuid::parse_str(&execution_id).unwrap())
            .fetch_one(&admin_db)
            .await
            .unwrap();
    assert_eq!(rollback_snapshot.as_deref(), Some("echo.rollback"));

    let tamper = sqlx::query(
        "UPDATE tool_executions SET rollback_tool_id = 'echo.rollback.changed' WHERE id = $1",
    )
    .bind(uuid::Uuid::parse_str(&execution_id).unwrap())
    .execute(&admin_db)
    .await;
    assert!(
        tamper.is_err(),
        "the database must reject post-dispatch policy snapshot mutation"
    );

    let rollback_request = json!({
        "execution_id": execution_id,
        "reason": "integration compensation"
    });
    let (status, rolled_back) = post_json(
        &router,
        "/tools.rollback",
        "tenant_a",
        "admin",
        rollback_request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "registered compensation executes");
    assert_eq!(rolled_back["status"], "executed");
    assert_eq!(rolled_back["tool_id"], "echo.rollback");
    let rollback_id = rolled_back["execution_id"].as_str().unwrap().to_string();
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0["params"]["name"], "echo.rollback");
        assert_eq!(
            calls[1].0["params"]["arguments"]["original_execution_id"],
            execution_id
        );
    }

    let (status, rollback_replay) = post_json(
        &router,
        "/tools.rollback",
        "tenant_a",
        "admin",
        rollback_request,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rollback_replay["execution_id"], rollback_id);
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "rollback executes exactly once"
    );

    let (status, _) = post_json(
        &router,
        "/tools.decide",
        "tenant_b",
        "admin",
        json!({"execution_id": execution_id, "decision": "approve"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "execution ids are tenant-isolated"
    );

    let revisions: Vec<(Option<i64>,)> = sqlx::query_as(
        "SELECT definition_revision FROM tool_executions
         WHERE tenant_id = 'tenant_a' ORDER BY created_at",
    )
    .fetch_all(&admin_db)
    .await
    .unwrap();
    assert_eq!(revisions, vec![(Some(1),), (Some(1),)]);

    let audit_actions: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT action FROM audit_events
         WHERE tenant_id = 'tenant_a' AND action LIKE 'tools.%' ORDER BY action",
    )
    .fetch_all(&admin_db)
    .await
    .unwrap();
    assert!(audit_actions.iter().any(|row| row.0 == "tools.decide"));
    assert!(audit_actions.iter().any(|row| row.0 == "tools.register"));
    assert!(audit_actions.iter().any(|row| row.0 == "tools.rollback"));
}
