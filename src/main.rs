//! Synapse service entrypoint.
//!
//! Boots configuration + telemetry, builds a (lazily-connected) Postgres pool,
//! assembles the [`synapse::app`] router, and serves it with tokio + axum.

use anyhow::Context as _;

use synapse::{app, config::Config, db, state::AppState, telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration first so telemetry can wire the OTLP exporter from it.
    let config = Config::from_env().context("failed to load configuration from environment")?;

    // Structured JSON logs always; OTLP trace and metric exporters are added when
    // `otel_endpoint` is set. Keep the guard alive until the process exits so buffered
    // telemetry is flushed on shutdown.
    let _otel_guard = telemetry::init(config.otel_endpoint.as_deref());
    tracing::info!(
        production_mode = config.production_mode,
        bind_addr = %config.bind_addr,
        db_max_connections = config.db_max_connections,
        max_request_body_bytes = config.max_request_body_bytes,
        request_timeout_secs = config.request_timeout_secs,
        max_in_flight_requests = config.max_in_flight_requests,
        embedding_model = %config.embedding_model,
        embedding_provider = config.embedding_provider.as_str(),
        otel_endpoint = ?config.otel_endpoint,
        "starting synapse"
    );

    // Make the caller-auth mode LOUD so a deployment that silently falls back to trusting
    // X-* identity headers can't hide. Mirror `JwtDecoder::from_config`'s precedence exactly
    // (JWKS endpoint > static RS256 public key > HS256 secret > trusted headers) so the boot log
    // matches the mode the extractor actually enforces — otherwise a correctly-secured verified-JWT
    // service would be mis-logged as insecure trusted-header mode.
    if let Some(jwks_url) = config.auth_jwks_url.as_deref() {
        tracing::info!(
            jwks_url = %jwks_url,
            audience = ?config.auth_jwt_audience,
            "caller auth: verified JWT — RS256 via JWKS endpoint (rotating public keys fetched by \
             kid; asymmetric, the service cannot mint tokens; X-* identity headers ignored)"
        );
    } else if config.auth_jwt_public_key.is_some() {
        tracing::info!(
            audience = ?config.auth_jwt_audience,
            "caller auth: verified JWT — RS256 (AUTH_JWT_PUBLIC_KEY set; asymmetric, the service \
             holds only the public key and cannot mint tokens; X-* identity headers ignored)"
        );
    } else if config.auth_jwt_secret.is_some() {
        tracing::info!(
            audience = ?config.auth_jwt_audience,
            "caller auth: verified JWT — HS256 (AUTH_JWT_SECRET set; X-* identity headers ignored)"
        );
    } else {
        tracing::warn!(
            "caller auth: TRUSTED-HEADER mode — X-Principal-Id/X-Tenant-Id/X-Role are trusted \
             VERBATIM. Run ONLY behind a trusted gateway. Set AUTH_JWT_PUBLIC_KEY (RS256) or \
             AUTH_JWT_SECRET (HS256) to require verified JWT bearer tokens."
        );
    }

    // Surface the resource-ABAC posture too, so a misconfigured/typo'd
    // ABAC_CONTEXT_OWNERSHIP (which fails fast at parse) or an operator who believes
    // it's on can verify it at boot.
    if config.abac_context_ownership {
        tracing::info!(
            "resource ABAC: context-ownership ENFORCED — a caller may access only their own context"
        );
    } else {
        tracing::info!(
            "resource ABAC: context-ownership OFF (default) — context access is governed by role \
             RBAC alone; set ABAC_CONTEXT_OWNERSHIP=true to restrict context to its owner"
        );
    }

    // `connect_lazy` so the process boots even if Postgres isn't up yet;
    // `/ready` gates traffic until the DB actually answers.
    let pool = db::init(&config)?;

    // Fail fast if the DB role can bypass RLS (SUPERUSER/BYPASSRLS silently voids
    // all tenant isolation). Tolerate a merely-unreachable DB here (the pool is
    // lazy on purpose); `/ready` re-checks fail-closed. Bounded so boot can't hang.
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        db::assert_rls_enforcing(&pool),
    )
    .await
    {
        Ok(Ok(())) => {
            tracing::info!("verified: database role enforces RLS (not superuser/bypassrls/owner)")
        }
        // Reachable DB + a role that can bypass RLS => refuse to start.
        Ok(Err(e @ (db::RlsCheckError::Privileged(_) | db::RlsCheckError::OwnerBypass { .. }))) => {
            return Err(anyhow::Error::new(e))
                .context("refusing to start with an RLS-bypassing database role");
        }
        // Merely unreachable at boot is tolerated (lazy pool); the gate + /ready re-check.
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "could not verify DB role at boot; gate + /ready will re-check")
        }
        Err(_elapsed) => {
            tracing::warn!("DB role RLS check timed out at boot; /ready will re-check")
        }
    }

    // Capture the worker settings before `config` is moved into the shared state.
    let worker_enabled = config.worker_enabled;
    let worker_poll_secs = config.worker_poll_secs;
    let worker_stale_secs = config.worker_stale_secs;

    let bind_addr = config.bind_addr.clone();
    let state = AppState::new(pool, config);

    // Opt-in background worker: reconciles runs stranded in `running` — a driver that crashed
    // mid-drive (fails a run left mid tool-execution; drives one stranded between steps) AND a
    // step whose retry backoff has elapsed. Off by default — it relies on the SECURITY DEFINER
    // discovery function `synapse_list_drivable_runs` (migration 0017) being owned by a
    // superuser/BYPASSRLS role. NOTE: step retries need this worker to progress, so a run with a
    // scheduled retry stays `running` until the worker drives it (like a crash orphan).
    if worker_enabled {
        synapse::orchestration::worker::spawn_worker(
            state.db.clone(),
            state.connector.clone(),
            state.embedder.clone(),
            state.config.embedding_model.clone(),
            worker_poll_secs,
            worker_stale_secs,
        );
        tracing::info!(
            poll_secs = worker_poll_secs,
            stale_secs = worker_stale_secs,
            "crash-recovery worker ENABLED"
        );
    } else {
        tracing::info!(
            "crash-recovery worker OFF (default); set WORKER_ENABLED=true to reconcile runs \
             orphaned by a driver crash"
        );
    }

    let router = app(state);

    let listener = tokio::net::TcpListener::bind(bind_addr.as_str())
        .await
        .with_context(|| format!("failed to bind {bind_addr}"))?;
    tracing::info!(addr = %bind_addr, "listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("http server error")?;

    // Falling out of `main` drops `_otel_guard`, flushing buffered traces and metrics.
    Ok(())
}

/// Resolve on SIGINT (ctrl-c) or SIGTERM so the server drains in-flight requests
/// and the telemetry guard flushes buffered spans on shutdown (e.g. k8s SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        // If the handler can't be registered (some restricted runtimes), do NOT
        // resolve — otherwise select! would fire immediately and shut the server
        // down on startup. Log and wait forever instead.
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to register SIGINT/ctrl-c handler");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to register SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining");
}
