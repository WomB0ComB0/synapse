//! Integration test for durable run orchestration (`POST /runs.start`, `/runs.resume`).
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set (CI stays database-free).
//! Each test provisions its own throwaway database via [`common`]. Run locally:
//!
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5459:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5459/postgres
//! cargo test --test runs_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use common::{app_pool, get_json, post_json, setup, swap_db};
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use synapse::db::tenant_tx;

#[tokio::test]
async fn runs_orchestration_start_resume_and_audit() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let router = setup(&base_url, "runs").await;

    // (a) a run with no human-approval callback completes synchronously.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "summarize", "input": {"doc": "x"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "auto run: {r}");
    assert_eq!(r["status"], "completed");
    assert!(
        r["resume_token"].is_null(),
        "completed run has no resume token: {r}"
    );
    println!("(a) auto run -> completed, no resume token");

    // (b) a human-gated run suspends `waiting` and hands back an opaque resume token.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "deploy", "workflow_id": "wf.deploy",
               "input": {"env": "prod"}, "callbacks": {"human_approval": true}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "waiting", "human-gated run suspends: {r}");
    let run2 = r["run_id"].as_str().unwrap().to_string();
    let token = r["resume_token"]
        .as_str()
        .expect("waiting run returns a token")
        .to_string();
    assert_eq!(token.len(), 64, "token is two hex UUIDs: {token}");
    println!("(b) human-gated run -> waiting + resume token");

    // (c) resuming with the correct token continues the run to completion.
    let (s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": run2, "token": token, "resume_input": {"approved_by": "user_9"}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resume: {r}");
    assert_eq!(r["status"], "completed");
    assert!(r["resume_token"].is_null());
    println!("(c) resume with valid token -> completed");

    // (d) the token is single-use: resuming again is a conflict, not a re-run.
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": run2, "token": token, "resume_input": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "double-resume -> 409");
    println!("(d) double-resume same token -> 409 (single-use)");

    // (e) a wrong token is indistinguishable from an unknown run (no probing).
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": run2, "token": "deadbeef".repeat(8), "resume_input": {}}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "wrong token -> 404");
    println!("(e) wrong token -> 404 (not an oracle)");

    // (f) validation: unknown run -> 404; non-UUID run_id -> 400; empty run_type/token -> 400.
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": "00000000-0000-0000-0000-000000000000", "token": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "unknown run -> 404");
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": "not-a-uuid", "token": "x"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "non-UUID run_id -> 400");
    let (s, _) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "   "}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty run_type -> 400");
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": run2, "token": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "empty token -> 400");
    println!("(f) validation: unknown->404, bad UUID->400, empty run_type/token->400");

    // (g) append-only, ordered event history: started, suspended, resumed, completed.
    //     Inspect run_events directly (superuser bypasses RLS) filtered by run_id.
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&swap_db(&base_url, "synapse_it_runs"))
        .await
        .expect("connect admin pool");
    let events: Vec<(i64, String)> = sqlx::query_as(
        "SELECT seq, event_type FROM run_events WHERE run_id = $1::uuid ORDER BY seq",
    )
    .bind(&run2)
    .fetch_all(&admin)
    .await
    .expect("query run_events");
    admin.close().await;
    let types: Vec<&str> = events.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(
        events.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "seq is dense+ordered"
    );
    assert_eq!(
        types,
        ["started", "suspended", "resumed", "completed"],
        "event history: {types:?}"
    );
    println!("(g) run_events: [started, suspended, resumed, completed] seq 1..4");

    // (h) tenant isolation: tenant_a's suspended run can't be resumed by tenant_b
    //     (RLS hides it -> 404), and tenant_b runs its own workflows independently.
    let (s, r) = post_json(&router, "/runs.start", "tenant_a", "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "deploy", "callbacks": {"human_approval": true}})).await;
    assert_eq!(s, StatusCode::OK);
    let run3 = r["run_id"].as_str().unwrap().to_string();
    let token3 = r["resume_token"].as_str().unwrap().to_string();
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_b",
        "user_2",
        json!({"run_id": run3, "token": token3, "resume_input": {}}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "tenant_b cannot resume tenant_a's run (RLS)"
    );
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_b",
        "user_2",
        json!({"tenant_id": "tenant_b", "run_type": "index"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "completed",
        "tenant_b's own run completes: {r}"
    );
    println!("(h) tenant isolation: tenant_b can't resume tenant_a's run; runs its own");

    // (i) audit trail (tenant-scoped): every governed start/resume is recorded with
    //     its outcome — success/require_approval for accepted calls AND `error` for
    //     failed ones (security item 4). tenant_a's accepted starts are exactly 1
    //     success + 2 require_approval, plus the failed empty-run_type start as
    //     `error`; 1 successful resume plus the failed resumes as `error`. tenant_b
    //     sees ONLY its own events (isolation): its auto start (success) and its
    //     one cross-tenant resume attempt (error) — never tenant_a's.
    let outcomes = |r: &serde_json::Value| -> Vec<String> {
        r["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["outcome"].as_str().unwrap().to_string())
            .collect()
    };
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=runs.start",
        "tenant_a",
        "user_1",
    )
    .await;
    let outs = outcomes(&r);
    assert_eq!(
        outs.iter().filter(|o| *o == "success").count(),
        1,
        "tenant_a starts: {outs:?}"
    );
    assert_eq!(
        outs.iter().filter(|o| *o == "require_approval").count(),
        2,
        "tenant_a starts: {outs:?}"
    );
    assert!(
        outs.iter().any(|o| o == "error"),
        "failed start audited as error: {outs:?}"
    );
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=runs.resume",
        "tenant_a",
        "user_1",
    )
    .await;
    let outs = outcomes(&r);
    assert_eq!(
        outs.iter().filter(|o| *o == "success").count(),
        1,
        "tenant_a resumes: {outs:?}"
    );
    assert!(
        outs.iter().any(|o| o == "error"),
        "failed resumes audited as error: {outs:?}"
    );
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=runs.start",
        "tenant_b",
        "user_2",
    )
    .await;
    assert_eq!(
        outcomes(&r),
        vec!["success"],
        "tenant_b sees only its own start"
    );
    let (_s, r) = get_json(
        &router,
        "/audit/events?action=runs.resume",
        "tenant_b",
        "user_2",
    )
    .await;
    assert_eq!(
        outcomes(&r),
        vec!["error"],
        "tenant_b sees only its own failed cross-tenant resume"
    );
    println!("(i) audit trail: accepted (success/require_approval) AND failed (error) outcomes, tenant-scoped");

    // ---- multi-step executor (the PR1 upgrade over hard-complete) ----
    // Admin pool (superuser bypasses RLS) to inspect run_steps/runs by run_id.
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&swap_db(&base_url, "synapse_it_runs"))
        .await
        .expect("connect admin pool");

    // (j) multi-step transform workflow -> completed; all steps completed in order.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "etl", "workflow_id": "pipeline.three"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "completed",
        "3-transform pipeline completes: {r}"
    );
    let jrun = r["run_id"].as_str().unwrap().to_string();
    let steps: Vec<(i32, String)> = sqlx::query_as(
        "SELECT step_index, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&jrun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        steps,
        vec![
            (0, "completed".into()),
            (1, "completed".into()),
            (2, "completed".into())
        ],
        "all 3 steps completed in order: {steps:?}"
    );
    println!("(j) multi-step transform pipeline -> completed, 3 steps completed in order");

    // (k) approval MID-workflow [transform, approval, transform]: start -> waiting
    //     AFTER step 0; resume -> continues through step 2 -> completed (resume advances).
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "review", "workflow_id": "review.gated"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "waiting",
        "gated review suspends at the approval step: {r}"
    );
    let krun = r["run_id"].as_str().unwrap().to_string();
    let ktoken = r["resume_token"].as_str().unwrap().to_string();
    let statuses: Vec<(i32, String)> = sqlx::query_as(
        "SELECT step_index, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&krun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        statuses,
        vec![
            (0, "completed".into()),
            (1, "waiting".into()),
            (2, "pending".into())
        ],
        "mid-workflow gate: step0 done, step1 waiting, step2 pending: {statuses:?}"
    );
    let (s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": krun, "token": ktoken, "resume_input": {"approved": true}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "completed",
        "resume ADVANCES through the final step: {r}"
    );
    println!("(k) mid-workflow approval -> waiting after step 0; resume advances to completed");

    // (l) two sequential gates -> two suspend/resume cycles with DISTINCT tokens.
    let (_s, r) = post_json(&router, "/runs.start", "tenant_a", "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "twogate", "workflow_id": "review.double-gated"})).await;
    assert_eq!(r["status"], "waiting");
    let lrun = r["run_id"].as_str().unwrap().to_string();
    let token1 = r["resume_token"].as_str().unwrap().to_string();
    let (_s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": lrun, "token": token1, "resume_input": {}}),
    )
    .await;
    assert_eq!(r["status"], "waiting", "second gate suspends again: {r}");
    let token2 = r["resume_token"].as_str().unwrap().to_string();
    assert_ne!(token1, token2, "each gate mints a fresh token");
    let (_s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": lrun, "token": token2, "resume_input": {}}),
    )
    .await;
    assert_eq!(
        r["status"], "completed",
        "second resume completes the run: {r}"
    );
    let (s, _) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": lrun, "token": token1, "resume_input": {}}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::CONFLICT,
        "the first (consumed) token can't be reused"
    );
    println!("(l) two gates -> two resume cycles, distinct tokens, first token single-use");

    // (m) failed-terminal: a workflow with a failing step -> run 'failed', later steps
    //     left pending, and a follow-up resume is 409 (not resumable).
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "faulty", "workflow_id": "pipeline.faulty"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "failed", "a failing step fails the run: {r}");
    let mrun = r["run_id"].as_str().unwrap().to_string();
    let (mstatus, merror): (String, Option<String>) =
        sqlx::query_as("SELECT status, error FROM runs WHERE run_id = $1::uuid")
            .bind(&mrun)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(mstatus, "failed");
    assert!(merror.is_some(), "runs.error is set on failure");
    let msteps: Vec<(i32, String)> = sqlx::query_as(
        "SELECT step_index, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&mrun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        msteps,
        vec![
            (0, "completed".into()),
            (1, "failed".into()),
            (2, "pending".into())
        ],
        "step0 done, step1 failed, step2 never reached: {msteps:?}"
    );
    println!("(m) failing step -> run failed, error set, later steps left pending");

    // (o) a governed Tool step (auto-approved): the tool executes INLINE and the run
    //     completes; the tool_executions row is 'executed' and linked to the run.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "deploy", "workflow_id": "tool.auto"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "completed", "auto tool run completes: {r}");
    let orun = r["run_id"].as_str().unwrap().to_string();
    let osteps: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT step_index, kind, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&orun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        osteps,
        vec![(0, "tool".into(), "completed".into())],
        "the tool step completed inline: {osteps:?}"
    );
    let (ostatus, otool, olinked): (String, String, bool) = sqlx::query_as(
        "SELECT status, tool_id, run_id IS NOT NULL FROM tool_executions WHERE run_id = $1::uuid",
    )
    .bind(&orun)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(ostatus, "executed", "auto tool recorded executed");
    assert_eq!(otool, "echo");
    assert!(olinked, "tool_executions is linked to the run (run_id set)");
    // The in-run tool call is ALSO recorded in the governed-action audit trail under
    // action='tool.execute' (so a compliance review of tool.execute sees run-driven calls).
    let (otx_audit,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events WHERE action = 'tool.execute' AND resource = 'echo' \
         AND outcome = 'success' AND metadata->>'run_id' = $1",
    )
    .bind(&orun)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        otx_audit, 1,
        "in-run tool.execute is audited under tool.execute"
    );
    println!("(o) auto Tool step executes inline -> run completed; tool_executions executed + run-linked + audited");

    // (p) a governed Tool step requiring approval: the run SUSPENDS `waiting` (the
    //     tool_execution is 'pending', NOT executed); resume approves + finalizes it
    //     (pending -> executed) and the run completes.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "deploy", "workflow_id": "tool.gated"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["status"], "waiting", "gated tool suspends the run: {r}");
    let prun = r["run_id"].as_str().unwrap().to_string();
    let ptoken = r["resume_token"].as_str().unwrap().to_string();
    let psteps: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT step_index, kind, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&prun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert_eq!(
        psteps,
        vec![
            (0, "transform".into(), "completed".into()),
            (1, "tool".into(), "waiting".into()),
            (2, "transform".into(), "pending".into()),
        ],
        "prepare done, tool waiting, notify not yet reached: {psteps:?}"
    );
    let (ppend, ptool): (String, String) =
        sqlx::query_as("SELECT status, tool_id FROM tool_executions WHERE run_id = $1::uuid")
            .bind(&prun)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(
        ppend, "pending",
        "gated tool recorded pending (NOT executed)"
    );
    assert_eq!(ptool, "deploy.prod");
    // Resume: approves the tool, finalizes it, and drives the run to completion.
    let (s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "user_1",
        json!({"run_id": prun, "token": ptoken}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r["status"], "completed",
        "resume completes the gated tool run: {r}"
    );
    let (pexec,): (String,) =
        sqlx::query_as("SELECT status FROM tool_executions WHERE run_id = $1::uuid")
            .bind(&prun)
            .fetch_one(&admin)
            .await
            .unwrap();
    assert_eq!(
        pexec, "executed",
        "resume finalized the tool_execution pending -> executed"
    );
    let psteps2: Vec<(i32, String)> = sqlx::query_as(
        "SELECT step_index, status FROM run_steps WHERE run_id = $1::uuid ORDER BY step_index",
    )
    .bind(&prun)
    .fetch_all(&admin)
    .await
    .unwrap();
    assert!(
        psteps2.iter().all(|(_, st)| st == "completed"),
        "all steps completed after resume: {psteps2:?}"
    );
    println!("(p) gated Tool step suspends (tool pending) -> resume approves+finalizes (executed) -> run completed");

    // (r) NULL-safety: a run whose principal is later DELETED (the runs FK is ON
    //     DELETE SET NULL principal_id — a run outlives its principal) must still be
    //     resumable; advance decodes a NULL principal_id as absent, not a 500. Start a
    //     human-gated run as a throwaway principal, delete it, then resume.
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "ghost_x",
        json!({"tenant_id": "tenant_a", "run_type": "ghost", "callbacks": {"human_approval": true}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ghost_run = r["run_id"].as_str().unwrap().to_string();
    let ghost_token = r["resume_token"].as_str().unwrap().to_string();
    sqlx::query("DELETE FROM principals WHERE tenant_id = 'tenant_a' AND principal_id = 'ghost_x'")
        .execute(&admin)
        .await
        .unwrap();
    let (s, r) = post_json(
        &router,
        "/runs.resume",
        "tenant_a",
        "ghost_x",
        json!({"run_id": ghost_run, "token": ghost_token}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "resume of a run whose principal was deleted must not 500: {r}"
    );
    assert_eq!(r["status"], "completed");
    println!(
        "(r) a run whose principal was deleted (principal_id NULL) still resumes -> completed"
    );

    // (q) the Tool step RE-CHECKS the tool.execute RBAC decision: a role authorized to
    //     START a run but DENIED tool.execute (a legitimate separation-of-duties policy)
    //     cannot invoke a tool via the run executor. Restrict the 'member' role in
    //     tenant_a to runs.start/runs.resume only (a populated role_permissions row set
    //     is that role's COMPLETE allowlist). Do this LAST — it changes member's policy.
    sqlx::query(
        "INSERT INTO role_permissions (tenant_id, role, action) VALUES \
            ('tenant_a','member','runs.start'), ('tenant_a','member','runs.resume')",
    )
    .execute(&admin)
    .await
    .unwrap();
    // user_1 (member) may start a run, but the auto Tool step's tool.execute is denied
    // -> the run is refused 403 and the whole tx rolls back (no run persists).
    let (s, r) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "deploy", "workflow_id": "tool.auto"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a tool step is refused for a role denied tool.execute: {r}"
    );
    // Sanity: a NON-tool run is still allowed (runs.start is granted).
    let (s, _) = post_json(
        &router,
        "/runs.start",
        "tenant_a",
        "user_1",
        json!({"tenant_id": "tenant_a", "run_type": "plain"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "a non-tool run is still allowed (runs.start granted)"
    );
    println!("(q) Tool step re-checks tool.execute RBAC: run refused for a role denied tool.execute; non-tool run still allowed");

    // (n) run_steps tenant isolation (RLS on the new table): a tenant_b-scoped
    //     tenant_tx sees ZERO of tenant_a's step rows; tenant_a definitely has some.
    let (a_steps,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM run_steps WHERE tenant_id = 'tenant_a'")
            .fetch_one(&admin)
            .await
            .unwrap();
    assert!(a_steps > 0, "tenant_a has run_steps (sanity)");
    let app = app_pool(&swap_db(&base_url, "synapse_it_runs"), "synapse_app_runs").await;
    let mut txb = tenant_tx(&app, "tenant_b").await.unwrap();
    let (leak,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM run_steps WHERE tenant_id = 'tenant_a'")
            .fetch_one(&mut *txb)
            .await
            .unwrap();
    assert_eq!(leak, 0, "tenant_b must not see tenant_a's run_steps (RLS)");
    drop(txb);
    admin.close().await;
    app.close().await;
    println!("(n) run_steps RLS: a tenant_b tenant_tx sees zero of tenant_a's step rows");

    println!("runs orchestration: multi-step executor (transform/approval/fail), resume-advances, distinct per-gate tokens, run_steps RLS, backward-compatible start/resume + audit all proven.");
}
