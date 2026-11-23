# Open Model Gateway: unreleased

Notes for changes on `main` after [v0.19.0](v0.19.0.md). They become the next release's notes.

## Pricing drafts

OpenRouter operator price previews apply their structural limits only to the catalog fields the mapper reads, so speech models with long voice lists and endpoints with many unread fields are priced instead of returned as all-unknown drafts; endpoint lists of up to 256 endpoints are compared (#741). Read fields keep their limits and an over-limit endpoint response is not cached. An endpoint with comparable rates and an unreadable status is reported when it is left out of the cheapest choice, the endpoint warnings state the input and output rates that were compared, and the warning that names the endpoint a draft was priced from is never omitted. Provider labels are reduced to printable ASCII and fall back to the endpoint tag. The catalog fetch runs one upstream request at a time and the cache holds at most 16 MiB. [Price suggestions](../governance-api.md#openrouter-price-suggestion)
