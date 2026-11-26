# Implementation Plan

## Ordered steps

1. **API-key tenant schema and publication spec**
   - Add optional tenant to `ApiKeyEntryConfig` and compiled `ApiKeyRef`/`ApiKeySpec`.
   - Reuse label validation with a tenant-specific field path; preserve duplicate key-name checks and publication-time credential materialization order.
   - Add config unit tests for omission, valid mapping, duplicate names, empty/over-bound/non-printable tenant values and unknown fields.

2. **Atomic identity tuple**
   - Extend `Identity` with optional tenant while preserving `Identity::new(subject, source)` semantics.
   - Add an explicit tenant constructor and context setter.
   - Add `PluginContext::set_identity_with_tenant` and `tenant()` with default implementations for existing alternate contexts; non-empty tenant writes must not silently disappear. Production ContextState overrides atomically with first-writer-wins and Authenticate-phase guard. Keep `set_identity` behavior unchanged.
   - Update the contract implementation, mock implementations and tests. Check source compatibility for internal and downstream test plugins.

3. **API-key authentication mapping**
   - Pass compiled tenant metadata to `ApiKeyPlugin` alongside materialized secrets.
   - On successful key match, atomically set key name as subject, optional tenant, and plugin instance as source.
   - Add API-key plugin tests verifying secret values do not appear in identity, logs or refusals.

4. **Tenant-aware token budget key**
   - Add one quota-identity derivation that selects tenant when present, otherwise subject, with distinct domain tags and the existing route scope.
   - Use the same derivation for check and charge.
   - Extend quota unit tests for same-tenant aggregation, subject fallback, tenant/subject domain separation, route isolation and post-hoc threshold semantics.

5. **WASM read-only tenant fact**
   - Add optional tenant to WASM `RequestFacts` and the JSON facts envelope.
   - Test absent tenant serializes as `null`, tenant is visible to the guest, and the WASM action contract cannot write identity or tenant.
   - Keep phase, action set, resource limits and dependency boundaries unchanged.

6. **End-to-end and compatibility coverage**
   - Add a server E2E with two API keys mapped to one tenant: one provider call consumes the shared route budget, the next key is refused before provider dispatch.
   - Add a route-isolation check and retain existing no-tenant API-key/JWT budget behavior.
   - Confirm publication failure leaves the LKG generation in service.
   - Update task documentation and only the backend spec sections that need a durable contract change.

## Validation

- Focused: `cargo test -p nico-plugins api_key`, `cargo test -p nico-http quota`, `cargo test -p nico-wasm`, and the relevant `nico-server` budget/WASM integration targets.
- Full: `cargo +nightly fmt --all --check`; `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`; `cargo test --workspace --all-features --locked`; `cargo deny check`; `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked`.
- Feature matrix: workspace without default features and traffic-only server build/test; verify `nico-core` and `nico-http` remain independent of `nico-wasm`/`wasmi`.
- Budget: `find crates -name '*.rs' | xargs wc -l | awk '$1 > 500 && $NF != "total" {print}'` must print nothing.
- Final review: `git diff --check`, inspect public API/ABI changes, assert LKG and compatibility behaviors, and keep this task's changes separate from the uncommitted G4 archive move.

## Review and rollback points

- After step 2, review the public `PluginContext` additive API and WASM mirror before proceeding to consumer changes.
- After step 4, run core/HTTP unit tests to ensure subject-only configurations retain the exact old quota key behavior.
- After step 5, verify the WASM ABI change is additive and no identity-writing action was introduced.
- If the identity contract cannot preserve existing plugin semantics, stop and revise the design before widening the interface further.
