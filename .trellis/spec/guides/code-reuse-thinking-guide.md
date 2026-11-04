# Code Reuse Thinking Guide

> **Purpose**: Stop and think before creating new code — does it already exist, and which crate
> owns the concept?

---

## The Problem

**Duplicated code is the #1 source of inconsistency bugs.**

When you copy-paste or rewrite existing logic:

- Bug fixes don't propagate
- Behavior diverges over time
- Codebase becomes harder to understand

In a Rust workspace this bites harder than usual: two copies of the same rule in different crates
cannot share a type, so they drift silently and the compiler never notices.

---

## Before Writing New Code

### Step 1: Search First

```bash
# Search for similar function/type names
rg "fn <name>" crates/

# Search for similar logic by concept
rg "<keyword>" crates/

# Search for the constant you are about to introduce
rg "<VALUE>" crates/
```

### Step 2: Ask These Questions

| Question                                   | If Yes...                                 |
| ------------------------------------------ | ----------------------------------------- |
| Does a similar function/type exist?        | Use or extend it                          |
| Is this pattern used elsewhere?            | Follow the existing pattern               |
| Could this be a shared utility?            | Put it in the crate that owns the concept |
| Am I copying code from another file?       | **STOP** — extract to the owner           |
| Would sharing this force a bad dependency? | Keep the duplication, and comment why     |

### Step 3: Pick The Right Home

Reuse does not mean "put it in `nico-core`". It means "put it in the crate that owns the
concept":

| Kind of logic                                              | Home             |
| ---------------------------------------------------------- | ---------------- |
| Protocol-agnostic (deadline, cancellation, error category)  | `nico-core`   |
| HTTP-specific (route match, header handling, upstream)      | `nico-http`   |
| AI-specific (model mapping, usage, SSE)                     | `nico-ai`     |
| Config loading/subscription                                 | `nico-config` |
| Built-in policy implementations                             | `nico-plugins`|

**The threshold is two real consumers, not three.** Promoting something into `nico-core`
requires two consumers in **different data planes**. One consumer plus "it feels generic" is not
enough — and "we might need it later" is not a consumer.

If sharing something would require a dependency edge that `../backend/directory-structure.md`
forbids, that is a design signal: the concept is in the wrong crate, or it genuinely belongs in
two places with different semantics.

---

## Common Duplication Patterns

### Pattern 1: Copy-Paste Functions

**Bad**: Copying a validation function into another crate.

**Good**: Extract it to the owning crate and import it. If importing it would create a forbidden
dependency edge, the function is probably in the wrong crate.

### Pattern 2: Similar Providers / Filters

**Bad**: Creating a new `Provider` implementation that is 80% identical to an existing one.

**Good**: Extend the existing implementation with a config knob, or extract the shared
request/response shaping into a private helper inside `nico-ai`.

### Pattern 3: Repeated Constants

**Bad**: The same default timeout, body limit, or header name defined in multiple crates.

**Good**: One `pub const` (or one config default) in the owning crate, imported everywhere.

Config defaults deserve special care: a limit that lives both as a `const` in `nico-http` and
as a `serde` default in `nico-server` is two owners for one rule. Pick the config document and
let the constant be derived from it, or vice versa — never both independently.

### Pattern 4: Repeated Payload Decoding

**Bad**: Two modules each index into the same raw JSON shape:

```rust
// BAD - each call site owns its own version of the provider contract
let model = body["model"].as_str().unwrap_or_default();
let prompt_tokens = body["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
```

> How to fix it — who defines the type, who decodes, who reduces stream events — is a **contract
> ownership** question and lives in
> [cross-layer-thinking-guide.md](./cross-layer-thinking-guide.md#mistake-4-every-consumer-parses-the-same-payload).
> This entry is only the duplication signal: **if the same untyped field is read in two places,
> create the shared decoder before adding a third reader.**

### Pattern 5: Repeated `match` Arms

**Bad**: The same variant-to-behavior mapping written in three places, drifting apart.

**Good**: One function that maps the enum, called from all three. If the mapping needs per-caller
variation, pass a parameter — do not fork the match.

---

## Exhaustive Matching Should Stay Exhaustive

When state is derived from an enum, prefer one `match` over scattered `if` checks — and keep the
match exhaustive.

```rust
// BAD - a catch-all arm hides newly added variants
match event {
    Event::Opened { .. } => handle_opened(),
    _ => handle_other(),   // new variant silently falls in here
}

// GOOD - every variant handled explicitly; adding one breaks the build
match event {
    Event::Opened { .. } => handle_opened(),
    Event::Comment { .. } => handle_comment(),
    Event::Status { .. } => handle_status(),
}
```

A wildcard `_` arm on a **locally defined** enum is a bug waiting for the next variant. Reserve
`_` for genuinely open sets: foreign types, and `#[non_exhaustive]` APIs from other crates.

---

## When to Abstract

**Abstract when**:

- The same code appears a **second** time and both call sites are real
- Logic is complex enough to have bugs
- Two different data planes genuinely need it

**Don't abstract when**:

- Only used once
- Trivial one-liner
- Abstraction would be more complex than duplication
- Sharing would require a dependency edge that violates `../backend/directory-structure.md`
- The two copies are similar today but answer different questions (they will diverge)
- It is a "just in case" generic layer with no second consumer

---

## After Batch Modifications

When you've made similar changes to multiple files:

1. **Review**: Did you catch all instances?
2. **Search**: Run `rg` to find any missed
3. **Consider**: Should this be abstracted now that there are two of them?

---

## Gotcha: Asymmetric Mechanisms Producing The Same Output

**Problem**: Two code paths must produce the same result — a "fast path" and a general path, or a
config-compile path and a test-fixture path. Structural changes only propagate through one.

**Symptom**: The general path works; the fast path silently drifts and returns stale or incomplete
data.

**Prevention**:

- **Best**: Eliminate the asymmetry — have the narrow path call the general one instead of
  maintaining its own copy.
- **If asymmetry is unavoidable** (usually for measured performance): add a test asserting both
  paths produce identical output for the same input, and keep that test next to the faster path.
- When changing the shared structure, search for every code path that references the old shape.

---

## Checklist Before Commit

- [ ] Searched for existing similar code
- [ ] No copy-pasted logic that should be shared
- [ ] No repeated untyped payload/JSON field extraction outside a shared decoder
- [ ] Constants and config defaults have exactly one owner
- [ ] Similar patterns follow the same structure
- [ ] Enum dispatch lives in one `match`, and the match has no unnecessary `_` arm
- [ ] Any new dependency edge is allowed by `../backend/directory-structure.md`
- [ ] Any new abstraction has two real consumers, not one plus optimism

---

**Core Principle**: Search before you write; put the shared thing where the concept lives, not
where it is convenient.
