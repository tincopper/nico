# Design: Virtual API Keys and Tenant Token Budgets

## 1. Boundary and invariants

1. `nico-plugins` owns the virtual API-key schema, bounded label validation, constant-time key verification and publication-time secret loading. `nico-http` owns the identity contract, quota key derivation and the request/accounting lifecycle. `nico-wasm` receives serialized facts only.
2. Identity is an atomic tuple `{subject, tenant?, source}`. `subject` remains the configured virtual-key name; `tenant` is a separate quota grouping dimension; `source` remains the authenticating plugin instance. First writer wins for the whole tuple.
3. Budget keying is `(route_id, tenant)` when tenant exists, otherwise `(route_id, subject)`. Source is attribution only. Route isolation, bounded in-process fixed windows, pre-dispatch check and post-response usage charge are unchanged.
4. A credential is materialized at publication and never included in identity, logs, WASM facts or error messages. Tenant IDs are identifiers, not secrets.
5. Existing `set_identity(subject)` remains source-compatible and writes `{subject, tenant: None, source}`. Existing API-key entries without tenant, JWT, custom plugin factories and documents without token budgets retain their current behavior.

## 2. Configuration and publication

Extend `ApiKeyEntryConfig` with optional `tenant: Option<String>` under each `config.keys[]` element. `deny_unknown_fields` remains enabled. Validate tenant using the existing API-key label validator and a dedicated `config.keys[].tenant` field path. Multiple entries may use the same tenant; repeated key names remain invalid.

Carry the optional tenant through `ApiKeySpec` beside name and secret reference. `ApiKeyFactory::build` continues to call `credentials::materialize_all` in entry order and passes secrets plus compiled tenant metadata to `ApiKeyPlugin`. Key matching remains constant-time; a successful match calls the new atomic identity setter with `subject = entry.name`, `tenant = entry.tenant`, and the plugin instance as source.

Publication remains generation-atomic: malformed tenant config or any secret materialization error refuses the candidate document and leaves the last-known-good generation serving.

## 3. Identity contract and WASM facts

Add optional tenant to `Identity`, preserving `Identity::new(subject, source)` as the existing no-tenant constructor and adding an explicit constructor for subject/tenant/source. Add `PluginContext::set_identity_with_tenant(subject, tenant)` with the same phase guard and first-writer-wins semantics as `set_identity`; default/legacy `set_identity(subject)` writes no tenant. Add read-only `PluginContext::tenant()` as a data-plane fact available wherever `identity()` is available.

The trait additions must have default implementations: `tenant()` defaults to `None`; `set_identity_with_tenant(subject, None)` delegates to the legacy setter; a non-empty tenant unsupported by an alternate context implementation returns an explicit execution error instead of silently dropping the grouping dimension. The production `ContextState` overrides the method and atomically writes the full identity tuple.

The trait remains an owned, framework-free contract. The WASM adapter mirrors the tenant fact into `RequestFacts` and the request facts JSON. `tenant` serializes as `null` when absent. The ABI gains a request-facts field but no new action, write capability, phase or secret access. Document the ABI compatibility implication in the task and pin it with host tests.

## 4. Quota key and accounting

Keep route `TokenBudget` policy and `BudgetStore` storage semantics. Change the shared key derivation input from a raw subject argument to the effective quota identity:

- tenant present: tenant is the quota identity;
- tenant absent: subject is the quota identity.

Hash `(route_id, effective quota identity)` with explicit domain separation between tenant and subject identifiers, so a tenant ID cannot collide semantically with an untenantized subject having the same bytes. The store still receives only a fixed-size hash. Both `check_request` and `charge_request` call the same key derivation function.

This does not create a tenant-global budget object. Each route's configured `token_budget` remains its own policy/window, and tenants share only among their virtual keys on that route. Key-level secondary budgets are not added.

## 5. Request and error flow

The API-key plugin runs in `Authenticate` before any `Limit` phase. It verifies the request key, then writes the complete identity tuple once. `upstream_peer` checks the existing route token budget against the tenant-aware key before provider selection/dispatch. Successful response recording charges reported input and output usage against the same key. Existing `429 token_budget_exceeded`, category, response body and post-hoc overshoot behavior remain unchanged.

No identity still means no token-budget check or charge. Tenant does not alter the request-count `rate_limit` plugin: it continues to use the subject or IP selector it currently implements.

## 6. Risks and rollout

- The ABI request-facts shape gains an optional tenant field. New host and guest artifacts must agree; old guest modules that ignore unknown JSON fields continue to work, but strict guest decoders may need a coordinated rebuild.
- A deployment with different process-local stores per replica has per-replica tenant windows, not one global window. Configuration and documentation must say so; shared persistence is a separate task with explicit atomicity and outage semantics.
- Tenant IDs are operator-controlled labels. Bound their length and character set, avoid placing them in metric labels, and do not include credential values in logs.
- No persisted state migration is needed: the budget store is process-local and its hash key is opaque. During one process lifetime, the new domain-separated key means a tenant-bound key starts a tenant window rather than inheriting the old subject window.
