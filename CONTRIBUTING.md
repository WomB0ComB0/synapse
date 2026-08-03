# Contributing to synapse

Thanks for your interest in improving **synapse**, an organizational brain for AI agents and team
workflows. This repository is
an **MVP skeleton**: the structure and contracts are in place, and most internals are thin stubs
with `TODO`s. That makes it a great place to pick up a well-scoped component and build it out.

## Ground rules

- Be respectful and constructive. By participating you agree to uphold a welcoming, harassment-free
  environment.
- All contributions are licensed under the project's [Apache License 2.0](LICENSE).
- Never commit secrets, credentials, or personal data. Copy `.env.example` to `.env` for local work;
  `.env` is git-ignored.

## Development setup

```bash
# Start Postgres (pgvector).
docker compose up -d db

# Configure the environment.
cp .env.example .env

# Run migrations (sqlx-cli; the sqlx::migrate! macro is intentionally not used).
cargo install sqlx-cli --no-default-features --features rustls,postgres
sqlx migrate run

# Build and test (tests are DB-free and run in CI without a database).
cargo build
cargo test
```

## Before you open a pull request

Run the same checks CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check        # advisories + license policy (see deny.toml)
```

- Keep changes focused and reviewable.
- Add or update tests for any behavior you change. Tests must stay **DB-free** so they pass in CI.
- If you change the API surface or a canonical schema, update `docs/`, `schemas/`, and `openapi/`
  in the same PR.
- Follow the design principles in the [README](README.md#what-is-an-organizational-brain): keep the
  brain outside any one agent, separate canonical data from derived retrieval artifacts, keep ACLs
  queryable, and require audit + approval before autonomous writes.

## Commit and PR conventions

- Write clear commit messages (imperative mood: "add retrieval reranker").
- Fill out the pull request template. Link any related issue or RFC.
- A maintainer (see [CODEOWNERS](.github/CODEOWNERS)) will review.

## Reporting bugs and requesting features

Open a GitHub issue with a clear title, reproduction steps (for bugs), and the expected vs. actual
behavior. For anything security-sensitive, follow [SECURITY.md](SECURITY.md) instead of filing a
public issue.
