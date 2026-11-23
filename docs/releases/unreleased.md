# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Development bootstrap

`bootstrap-dev` (and `bootstrap-demo`, which shares the check) now refuses any installation already in use: accounts, grants, workspaces, keys, catalog entries, audit or execution history, or a configuration change since migration (#743). A configuration-only database is no longer seeded with a platform admin. A repeat run on an installation with its own bootstrap marker is unchanged. The refusal for a non-literal loopback host (for example `localhost`) now says to write `127.0.0.1` or `[::1]` in `DATABASE_URL`. A concurrent first OIDC sign-in is serialized with the seed by the catalog and installation locks both already take; tests now cover both orders. A loopback forwarder to a freshly migrated remote database still passes the guard. [Local demo](../local-demo.md)
