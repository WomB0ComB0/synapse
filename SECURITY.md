# Security Policy

**synapse** is a governance layer — permission-aware retrieval, policy-guarded tools, and audited
workflows — so we take security reports seriously. Thank you for helping keep synapse and its users safe.

## Supported versions

This repository is pre-1.0 and under active development. Security fixes target the `main` branch.
There is no supported release line yet; pin deployments to a reviewed commit or image digest.

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Use GitHub private vulnerability reporting (the **Report a vulnerability** action under the
repository Security tab). Do not send reports to placeholder or unverified email addresses.

Please include:

- A description of the issue and its potential impact.
- Steps to reproduce (proof-of-concept if possible).
- Affected component/endpoint and commit hash.
- Any suggested remediation.

## What to expect

- **Acknowledgement** within 3 business days.
- A **triage assessment** and severity rating within 10 business days.
- Coordinated disclosure: we will agree on a timeline with you and credit you in the advisory unless
  you prefer to remain anonymous.

## Scope and hardening notes

Because synapse mediates access to organizational knowledge and tools, we are especially interested
in reports concerning:

- Authorization / ACL bypass (principal, tenant, team, or row-level scoping).
- Policy-gateway bypass allowing unapproved tool execution or autonomous writes.
- Retrieval leakage across tenants or teams.
- PII exposure or failures to separate personal memory from org knowledge.
- Audit-log tampering or gaps.
- Dependency and supply-chain issues (see `deny.toml` and the `security` workflow).

Automated dependency and code scanning run via `.github/workflows/security.yml` and Dependabot.
