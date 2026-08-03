//! Live integration test: the explicit teams API — `teams.create`, `teams.list`, `teams.members`.
//!
//! Complements `team_authority_it.rs` (which covers add/remove authority + group-grant retrieval):
//!   * create   — admin-only; a re-create is a 409; a non-admin is 403 (squatting closed).
//!   * members  — owner/admin-only; a missing team is 404-to-admin / 403-to-non-admin (no oracle).
//!   * list     — scoped: an admin sees all teams, a member sees only theirs, others see none.
//!
//! **DB-gated:** skipped unless `DATABASE_URL` is set. Run locally:
//! ```bash
//! docker run --rm -d -e POSTGRES_PASSWORD=postgres -p 5474:5432 pgvector/pgvector:pg16
//! export DATABASE_URL=postgres://postgres:postgres@localhost:5474/postgres
//! cargo test --test teams_api_it -- --nocapture
//! ```

mod common;

use axum::http::StatusCode;
use axum::Router;
use common::{post_json, provision_role, setup_with_db};
use serde_json::{json, Value};

fn team_ids(resp: &Value) -> Vec<String> {
    resp["teams"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["team_id"].as_str().unwrap().to_string())
        .collect()
}

async fn list_teams(router: &Router, tenant: &str, caller: &str) -> Vec<String> {
    let (s, r) = post_json(router, "/teams.list", tenant, caller, json!({})).await;
    assert_eq!(s, StatusCode::OK, "teams.list as {caller}: {r}");
    team_ids(&r)
}

#[tokio::test]
async fn teams_create_list_members_api() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let t = "tenant_a";
    let (router, admin) = setup_with_db(&base_url, "teamsapi").await;
    // Team CREATE/manage authority is DB-authoritative admin (not header-assertable).
    provision_role(&admin, t, "boss", "admin").await;

    // --- create: an admin creates a team, owned by the caller ---
    let (s, r) = post_json(
        &router,
        "/teams.create",
        t,
        "boss",
        json!({"team_id": "alpha", "name": "Alpha Squad"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "admin creates alpha: {r}");
    assert_eq!(r["status"], "created");

    // re-create the same team -> 409 Conflict (explicit create is NOT idempotent)
    let (s, r) = post_json(
        &router,
        "/teams.create",
        t,
        "boss",
        json!({"team_id": "alpha"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "re-create alpha is a 409: {r}");
    assert_eq!(r["error"]["code"], "conflict");

    // a non-admin cannot create (team-namespace squatting stays closed)
    let (s, r) = post_json(
        &router,
        "/teams.create",
        t,
        "mallory",
        json!({"team_id": "beta"}),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "non-admin create -> 403: {r}");
    assert_eq!(r["error"]["code"], "forbidden");
    println!("(create) admin creates+owns a team; a re-create is 409; a non-admin is 403");

    // seed a member so `members` + `list` scoping have something to show
    let (s, r) = post_json(
        &router,
        "/teams.add_member",
        t,
        "boss",
        json!({"team_id": "alpha", "principal_id": "alice", "role": "member"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "boss adds alice to alpha: {r}");

    // --- members: an owner/admin sees the roster ---
    let (s, r) = post_json(
        &router,
        "/teams.members",
        t,
        "boss",
        json!({"team_id": "alpha"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "boss lists alpha members: {r}");
    assert!(
        r["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["principal_id"] == "alice" && m["role"] == "member"),
        "alice (role=member) is in the roster: {r}"
    );

    // a non-owner non-admin cannot list members
    let (s, r) = post_json(
        &router,
        "/teams.members",
        t,
        "mallory",
        json!({"team_id": "alpha"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "mallory can't list alpha members: {r}"
    );

    // a missing team: 404 to an admin, the SAME 403 to a non-admin (no existence oracle)
    let (s, _) = post_json(
        &router,
        "/teams.members",
        t,
        "boss",
        json!({"team_id": "ghost"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "admin gets 404 for a missing team"
    );
    let (s, _) = post_json(
        &router,
        "/teams.members",
        t,
        "mallory",
        json!({"team_id": "ghost"}),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::FORBIDDEN,
        "a non-admin gets 403 (no existence oracle)"
    );
    println!("(members) owner/admin sees the roster; non-owner=403; missing team=404-admin/403-non-admin");

    // --- list: scoped visibility ---
    let (s, _) = post_json(
        &router,
        "/teams.create",
        t,
        "boss",
        json!({"team_id": "gamma"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "boss creates gamma");

    // an admin sees ALL of the tenant's teams
    let boss_view = list_teams(&router, t, "boss").await;
    assert!(
        boss_view.contains(&"alpha".to_string()) && boss_view.contains(&"gamma".to_string()),
        "admin sees all teams: {boss_view:?}"
    );

    // alice is a MEMBER of alpha (not gamma) -> sees only alpha
    let alice_view = list_teams(&router, t, "alice").await;
    assert!(
        alice_view.contains(&"alpha".to_string()),
        "alice sees alpha (member): {alice_view:?}"
    );
    assert!(
        !alice_view.contains(&"gamma".to_string()),
        "alice does NOT see gamma: {alice_view:?}"
    );

    // mallory owns/joins nothing -> sees no teams (no namespace enumeration)
    let mallory_view = list_teams(&router, t, "mallory").await;
    assert!(
        mallory_view.is_empty(),
        "mallory sees no teams: {mallory_view:?}"
    );
    println!("(list) admin sees all; a member sees only their teams; a non-member sees none");

    admin.close().await;
    println!("teams API: create (admin-only, 409 on dup), members (owner/admin, 404/403 parity), list (scoped: admin=all / member=own / else none).");
}

/// Drift-guard (0023): the `role_permissions.action` CHECK vocabulary MUST accept every
/// `Action::as_str()`. Otherwise, because `role_permissions` uses replace-semantics, a tenant that
/// configures the table can neither keep nor re-grant a missing action (a `23514` on INSERT) —
/// silently locking the role out with no remediation. This asserts the CHECK covers the whole enum,
/// so a future `Action` addition that forgets the migration fails HERE instead of in production.
#[tokio::test]
async fn role_permissions_check_accepts_every_action_label() {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("DATABASE_URL unset — skipping DB-gated integration test");
        return;
    };
    let (_router, admin) = setup_with_db(&base_url, "teamscheck").await;
    for a in synapse::auth::policy::Action::ALL {
        let r = sqlx::query(
            "INSERT INTO role_permissions (tenant_id, role, action) \
             VALUES ('tenant_a', 'member', $1) ON CONFLICT DO NOTHING",
        )
        .bind(a.as_str())
        .execute(&admin)
        .await;
        assert!(
            r.is_ok(),
            "role_permissions_action_check must accept '{}' — add it to the action CHECK migration: {:?}",
            a.as_str(),
            r.err()
        );
    }
    admin.close().await;
    println!("role_permissions action CHECK accepts every Action::as_str() (no vocabulary drift)");
}
