# Thinking Guides

> **Purpose**: Expand your thinking to catch things you might not have considered.

---

## Why Thinking Guides?

**Most bugs and tech debt come from "didn't think of that"**, not from lack of skill:

- Didn't think about what happens at a crate boundary → cross-layer bugs
- Didn't think about code patterns repeating → duplicated code everywhere
- Didn't think about edge cases → runtime errors
- Didn't think about future maintainers → unreadable code

These guides help you **ask the right questions before coding**.

---

## Available Guides

| Guide | Purpose | When to Use |
|-------|---------|-------------|
| [Code Reuse Thinking Guide](./code-reuse-thinking-guide.md) | Find the owner of a concept instead of writing it twice | When you notice repeated patterns |
| [Cross-Layer Thinking Guide](./cross-layer-thinking-guide.md) | Decide who owns decoding, validation, and error mapping at each boundary | Features spanning multiple crates |

Layer-specific conventions live outside this directory:

| Directory | Contents |
|-----------|----------|
| `../backend/` | Rust workspace rules: crate boundaries, errors, config store, logging, quality gates |
| `../frontend/` | Component, hook, state, and type conventions |

**Division of labour**: this directory is about *thinking* — the questions to ask before
writing code. `../backend/` and `../frontend/` are about *rules* — what the code must look like.
If an item here could be restated as a lint or a checklist item in a layer spec, it belongs there
instead.

---

## Quick Reference: Thinking Triggers

### When to Think About Cross-Layer Issues

- [ ] Feature touches 3+ crates (config, core, data plane, plugins)
- [ ] Data format changes between crates
- [ ] Multiple consumers need the same data
- [ ] You're not sure which crate owns some logic
- [ ] You are adding a config field, an SSE frame field, or a provider response field
- [ ] A consumer starts reading raw JSON / `serde_json::Value` fields directly
- [ ] You are adding a new `Provider`, `Plugin` kind, `Upstream`, or `ConfigSource` implementation
- [ ] You are changing something that crosses the reload boundary (config → `RuntimeSnapshot` → hot path)
- [ ] You are touching streaming / incremental parsing (`ProviderStream`)

→ Read [Cross-Layer Thinking Guide](./cross-layer-thinking-guide.md)

### When to Think About Code Reuse

- [ ] You're writing similar code to something that exists
- [ ] You see the same pattern repeated a second time
- [ ] You're adding a new field to multiple places
- [ ] **You're modifying any constant or config default**
- [ ] **You're creating a new utility/helper function** ← Search first!
- [ ] Two modules decode the same payload shape
- [ ] Multiple branches update the same derived state from an enum
- [ ] You are tempted to put something into `nico-core` "because it feels generic"

→ Read [Code Reuse Thinking Guide](./code-reuse-thinking-guide.md)

### When Verifying AI Cross-Review Results

- [ ] Reviewer claims "user input can be malicious" → Check the actual data source (internal config file? untrusted upstream response? client request?)
- [ ] Reviewer flags "missing validation" → Is the data already validated at the compile boundary?
- [ ] Reviewer says "behavior change" → Read the code comments — is it intentional design?
- [ ] Reviewer identifies a "bug" in a test → Mentally delete the feature being tested — does the test still pass? If yes → tautological test

**Common AI reviewer false-positive patterns**:

1. **Trust boundary confusion**: Treating internally compiled config as untrusted external input
2. **Ignoring design comments**: Flagging intentional behavior documented in code comments as bugs
3. **Variable misreading**: Not tracing a variable to its actual definition
4. **Clone/lifetime nitpicking**: Demanding a refactor that fights the borrow checker without a measured need

**Verification rule**: Every CRITICAL/WARNING finding must be verified against the actual code
before prioritizing. Budget ~35% false-positive rate for AI reviews.

---

## The "Search Before You Change" Habit

Before modifying anything that could be duplicated:

```bash
# Search for the value you're about to change
rg "value_to_change" crates/
```

This single habit prevents most "forgot to update X" bugs — especially for resource limits and
config defaults, which are often duplicated between the compile step and the data plane.

---

## Using These Guides

1. **Before coding**: Skim the relevant thinking guide
2. **During coding**: If something feels repetitive or complex, check the guides
3. **After bugs**: Add new insights to the relevant guide (learn from mistakes)

`trellis-break-loop` writes back into these files on purpose: a bug that recurs is a missing
checklist item, not a missing fix.

---

## Contributing

Found a new "didn't think of that" moment? Add it to the relevant guide as a concrete checklist
item or a "Bad / Good" example using this project's types.

Keep entries **project-specific and non-duplicated**. A rule that can be enforced by the compiler,
clippy, or a lint belongs in `../backend/quality-guidelines.md`, not here. Before adding an item,
check whether the other guide already covers it — the two files are meant to be disjoint:

| Guide | Owns |
|---|---|
| Code Reuse | "we wrote the same thing twice" → finding the owner |
| Cross-Layer | "this boundary has no owner" → contracts, decoding, validation |

---

**Core Principle**: 30 minutes of thinking saves 3 hours of debugging.
