# Local enterprise demo

The demo is development-only and passwordless. Never register its issuer with a real deployment or point it at existing data. It uses a new Compose project/volume and the dedicated `gateway_enterprise_demo` database; it does not reset, migrate or reseed legacy databases.

## Accounts

Open **http://127.0.0.1:3000**, select SSO sign-in, then choose an account:

| Account | Email | Access |
| --- | --- | --- |
| Platform Admin | `operator@demo.invalid` | Installation configuration, shared-workspace administration and financial totals; no foreign personal keys/details |
| Platform Auditor | `auditor@demo.invalid` | Read-only Admin configuration, audit and aggregate costs; own personal workspace |
| Team Admin · Alex | `alex@demo.invalid` | Own personal workspace; owner/administrator of Product Team and Research Project |
| Platform User · Blair | `blair@demo.invalid` | Own personal workspace; member activity/own keys in Product and Research |
| Unentitled SSO user | `unentitled@demo.invalid` | Authentication succeeds, platform access is denied; no personal workspace |

Sign out through the account menu and sign in again to switch personas. Platform role and workspace membership are independent. Admin/Auditor can switch Workspace/Admin portals; Alex and Blair have Workspace only.

The issuer signs explicit group claims. Rust reconciles them against persisted mappings, preserving separate manual memberships. Alex's owner grants demonstrate that a lower group-derived membership does not overwrite a manual source. Personal workspaces are created only on actual entitled sign-in—not by fixture seeding.

## Start a fresh demo

Run from the repository root. If a matching database already exists, preserve it; do not delete/reset a volume to obtain new fixtures.

```sh
npm ci
docker compose -p omg-enterprise-demo -f deploy/demo/compose.yaml up -d --wait postgres
mkdir -p .local/enterprise-rebuild
cp .env.demo.example .local/enterprise-rebuild/demo-bootstrap.env
chmod 600 .local/enterprise-rebuild/demo-bootstrap.env
set -a
source .local/enterprise-rebuild/demo-bootstrap.env
set +a
cargo build -p open-model-gateway
./target/debug/open-model-gateway migrate
./target/debug/open-model-gateway bootstrap-demo
python3 scripts/demo-runtime.py
npm run build --workspace @omg/web -- --outDir ../../.local/enterprise-rebuild/web-dist
```

In one terminal:

```sh
OMG_ENABLE_LOCAL_DEMO=1 node scripts/demo-oidc.mjs
```

In another, source the private, restricted runtime environment (not the bootstrap account):

```sh
set -a
source .local/enterprise-rebuild/demo-runtime.env
set +a
./target/debug/open-model-gateway serve
```

The tracked demo Compose file binds PostgreSQL to **127.0.0.1:54349**, separate from the disposable regression cluster on 54339 and legacy development PostgreSQL on 54329. The gateway and issuer bind 3000 and 18084 respectively. There is no production login bypass.

`GATEWAY_ENV_FILE` selects the demo environment instead of implicitly loading an existing root `.env`. Unset `GATEWAY_OIDC_CLIENT_SECRET` before launching this public demo client. The runtime helper writes a 0600 private environment, refuses existing roles/files, and verifies the explicit runtime grants with rollback-only probes. Never print or commit that file. Serving uses this restricted role, not the bootstrap account. The test suite's explicit `runtime_privileges` probe verifies the allowlist using rollback-only fixtures on a disposable cluster; do not run that role/maintenance-ACL probe against real PostgreSQL.

## What to test

- Workspace/Admin switching, grouped Personal/Teams/Projects selector, breadcrumbs, responsive sidebar, color mode and command palette.
- Create shared workspaces from Admin, not the switcher. Membership in one Team does not grant access to a sibling Project.
- Create/revoke/rotate your own human keys, inspect one-time disclosure without recording secrets, and test service-account keys in shared workspaces. Global metadata authority without membership cannot mint a human key.
- Compare type catalog inheritance with Research's replacement override. Catalog availability enables self-service selection, not automatic selection of every model. Direct assignments survive unrelated catalog loss; retired key selections never revive.
- Configure live limits, immutable price versions, explicit model protocols and optional cost centers. Assignments affect future admission snapshots, not historical records.
- Verify Blair cannot manage Product or view Alex's activity, and Auditor cannot mutate platform configuration or open foreign personal details.
- Check empty cost reports and coverage labels. No fake request/cost data is seeded.

Example connections/deployments start **disabled**. Local HTTP approval is explicit in `.env.demo.example`; approval does not mean a server exists or a model is certified. Example prices are not vendor prices. Configure an actual local endpoint/model and enforced input bounds before enabling inference. Paid-provider tests require separate explicit approval and server-held credentials.

`bootstrap-demo` is transactional and idempotent after its own successful seed marker. It preserves edits, inactive users and history; on any other installation it applies the same emptiness check as `bootstrap-dev` (below) and refuses. Its database URL must name the literal loopback address `127.0.0.1` or `[::1]`, with no Unix socket (`?host=/...`); the same check applies to `serve` whenever the demo issuer is configured. The old organization-persona upgrade flag is unsupported. Restart issuer and gateway together: the demo signing key is ephemeral, and a new issuer key is picked up only after a JWKS refresh (rate-limited to once a minute for unknown key IDs).

To stop without deleting data:

```sh
docker compose -p omg-enterprise-demo -f deploy/demo/compose.yaml stop
```

Stopping the issuer/gateway is separate. Do not use `down -v` as a recovery or upgrade procedure.

`bootstrap-dev` is a separate minimal development fixture. It requires
`GATEWAY_ENV=development` and a TCP database connection whose host is the
literal address `127.0.0.1` or `::1` (written `[::1]` in a URL). A host name
such as `localhost` is refused, and so are Unix socket overrides and `PGHOST` or
`PGHOSTADDR` values (used only when the URL names no host) that are not one of
those addresses, all before connecting. Tests that call `bootstrap::seed` need
the same kind of `DATABASE_URL`. On a fresh, explicitly selected migrated
database it creates an admin and prints two inference keys once; keep that
terminal output private.

It refuses an installation that is already in use (`bootstrap.rs`
`ensure_empty`, which `bootstrap-demo` shares): a row in any table either seed
writes (accounts, platform role and membership grants, OIDC group mappings,
cost centers, workspaces, service accounts, keys, provider connections, models,
deployments and prices, catalogs and their assignments, model grants, budgets,
audit events), any execution history, or a configuration topic that has moved
since migration (`config_versions`; catalog, policy, key-selection and settings
writes bump it, as do updates and deletions of accounts, grants, workspaces and
keys, but not their inserts). The only exception is a repeat run: when the
installation carries its own successful bootstrap marker and developer
identity, the command makes no changes and never prints or rotates existing
keys. It does not reset databases or enable upstream providers.

What the check does not see: state only in other tables, and a loopback
forwarder (an SSH tunnel or `kubectl port-forward`) to a freshly migrated
remote database, which looks like a local server. Point `bootstrap-dev` only at
a database you created for development.
