# Quality Guidelines

> Linting, testing, accessibility, and review standards for the frontend.

---

## Overview

Quality is enforced by tools first, review second. Every rule that can be a lint is a lint.

Baseline commands — all must pass before a task is reported done:

```bash
tsc --noEmit                 # strict type check, no emit
eslint . --max-warnings 0    # zero warnings tolerated
vitest run                   # unit + component tests
```

Add when the project has them configured: `prettier --check .`, `playwright test` (critical paths),
and a bundle-size budget check in CI.

---

## Toolchain Configuration

- **TypeScript**: `strict` plus `noUncheckedIndexedAccess`, `exactOptionalPropertyTypes`,
  `noImplicitOverride`, `noFallthroughCasesInSwitch`, `verbatimModuleSyntax`. See `type-safety.md`.
- **ESLint** (flat config) with: `typescript-eslint` strict + type-checked rules, `react`,
  `react-hooks` (`rules-of-hooks` and `exhaustive-deps` as **errors**), `jsx-a11y`,
  `import` (no cycles, no restricted paths), and a naming/consistency ruleset.
- **Formatting**: Prettier is the single formatter; ESLint does not own formatting rules.
- **Tests**: Vitest + React Testing Library for unit/component; Playwright for end-to-end.
- No `eslint-disable` without an inline reason. `eslint-disable` at file scope is forbidden.

---

## Forbidden Patterns

| Pattern | Why | Instead |
|---|---|---|
| `any`, `as` on external data, `!` non-null | Compiler lie that crashes at runtime | Zod parse / explicit narrowing |
| `@ts-ignore` | Suppresses errors indefinitely | Fix the type, or `@ts-expect-error` with a reason |
| `eslint-disable` file-wide or for type rules | Debt accumulates silently | Fix the code; narrow local `expect`/disable with reason |
| `useEffect` for derived state or data fetching | Extra renders, duplicated caching, stale data | Compute in render / query hook |
| Fetching inside a presentational component | Untestable, couples UI to transport | Query hook in `features/<f>/api` |
| `dangerouslySetInnerHTML` with unsanitized input | XSS | Render as text, or sanitize via vetted library |
| `key={index}` in lists | Wrong row state on reorder | Stable domain id |
| Global store for single-component state | Indirection without benefit | Local state |
| `console.log` / `debugger` committed | Noise, leaks data into production console | Remove; use the error reporter |
| `setTimeout` to wait for state | Race conditions | Await the actual signal / `waitFor` in tests |
| Class components, `forwardRef`, `defaultProps` | Legacy patterns, not React 19 idiom | Function components, `ref` as a prop, defaults in signature |
| Non-semantic clickable `<div>` | Keyboard/screen-reader inaccessible | `<button>` / `<a>` |
| Removing focus outlines without replacement | Keyboard users lose their place | Visible focus indicator |
| Color-only state signalling | Fails WCAG 1.4.1 | Add text/icon |
| Blanket `useMemo`/`useCallback` | Noise, fights the compiler when it is enabled | Memoize with a stated reason |
| `useMemo` around a primitive expression | Hook + comparison costs more than the expression | Inline it |
| `useEffect` modeling an event (state + effect) | Re-runs on unrelated changes, duplicate side effects | Do it in the handler |
| Event listener per component instance | N listeners for one global event | Module-level listener + callback set |
| Scroll listener without `{ passive: true }` | Blocks scrolling | Passive listener |
| Sequential awaits for independent data | 3 round trips instead of 1 | `Promise.all` |
| Barrel import of a large library entry point | 200–800 ms cold-start cost, slower builds | Import from source, or configure `optimizePackageImports` |
| Heavy component imported eagerly | Inflates the initial bundle and TTI | Dynamic `import()` / `React.lazy` |
| `{count && <X />}` where `count` can be `0` | Renders a literal `0` | `{count > 0 ? <X /> : null}` |
| Optimizing without a measured baseline | Adds complexity for no proven gain | Measure → change → re-measure |
| Copy-pasted component with tweaks | Divergence | Variant prop or composition |
| Three interacting boolean props | 2^n impossible states | Explicit variant components |
| Sibling state synced via `useEffect`/ref | Fragile ordering, missed updates | Lift state into a provider |
| Feature importing another feature's internals | Coupling, cycles | Promote to shared layer |
| New dependency without justification | Supply-chain and bundle cost | Standard library / existing stack |

---

## Required Patterns

- **Composition over configuration**: `children` and explicit slots before ever-growing prop lists;
  explicit variants before boolean matrices; state in a provider before prop drilling.
- **Discriminated unions for async and variant state**; no boolean matrices.
- **Schema-validated boundaries** for every network/storage/URL input.
- **Explicit loading, empty, error, and success states** for every async surface.
- **Semantic HTML and keyboard operability** for every interactive element.
- **Stable identities** for list keys, effect dependencies, and query keys.
- **Functional `setState`** when the next value depends on the previous value.
- **Primitive effect dependencies**, not objects.
- **Cleanup for every subscription, timer, and in-flight request.**
- **Named exports** for components and hooks (consistent, refactor-friendly).
- **Accessibility check in the definition of done** — keyboard path verified before review.
- **Performance change requires a measurement** — see `performance-guidelines.md`.
- **Tests assert behavior, not implementation**: query by role/label/text, not by class or test id
  unless no semantic handle exists.

---

## Testing Requirements

Stack: **Vitest + React Testing Library** for unit/component, **Playwright** for critical flows,
**MSW** for network mocking at the HTTP boundary.

Layout:

- `features/<f>/__tests__/` or `*.test.tsx` beside the source.
- Shared fixtures and MSW handlers in `src/test/`.

Rules:

- Test **behavior through the public interface**: render the component, interact as a user, assert
  visible outcomes. No shallow rendering, no asserting internal state.
- Query priority: `getByRole` → `getByLabelText` → `getByText` → `getByTestId` (last resort). Role
  queries double as an accessibility check.
- Every async surface has tests for all four states (loading, empty, error, success) where the
  component owns them.
- `user-event` for interaction, never `fireEvent` for typing/clicking.
- No `sleep`/arbitrary timeouts; use `findBy*`/`waitFor` and fake timers for debounce logic.
- Mock at the network boundary (MSW), not by mocking your own hooks — mocking hooks tests the mock.
- Snapshot tests are allowed only for stable, small, semantic output; never for whole pages.
- Playwright covers: auth flow, primary navigation, one full happy-path per critical feature, and
  the most likely failure (network error surfaced to the user).
- Accessibility: run `axe` in component tests where practical, and do a manual keyboard pass on any
  new interactive surface. Screen-reader verification is required for new dialogs, menus, and
  live-updating regions.
- Coverage expectations: new hooks and pure logic in `lib/` should be near-complete; components at
  least cover each rendered state. Do not chase a global percentage.
- Performance tests are not unit tests: measure with the profiler and record the baseline in the PR,
  rather than asserting frame timings in CI.

---

## Code Review Checklist

- [ ] Does the change respect the layer rules (`features/*` → shared only, no cross-feature imports)?
- [ ] Is any new abstraction backed by two real consumers? If not, inline it.
- [ ] Are types derived from schemas rather than asserted? Any new `as`/`any`/`!`?
- [ ] Is every async surface showing loading, error, empty, and success states?
- [ ] Is state in the right category (derived / local / server / URL / global)?
- [ ] Does every effect synchronize with an external system, with cleanup, and depend on
      primitives rather than objects?
- [ ] Is any event modeled as state + effect, or any global listener duplicated per instance?
- [ ] Are list keys stable domain ids?
- [ ] Keyboard path works: tab order, focus visible, Escape closes overlays, focus restored?
- [ ] Labels present for inputs and icon-only buttons? Dialogs titled? Contrast and hit targets
      adequate?
- [ ] Any new dependency? Justified, maintained, and bundle-cost acceptable?
- [ ] Any new barrel import or eagerly imported heavy component?
- [ ] Any sequential awaits that could be parallel?
- [ ] Is the memoization decision consistent with whether React Compiler is enabled?
- [ ] Any `{value && ...}` conditional where `value` can be `0` or `NaN`?
- [ ] Do tests cover the failure path, not just the happy path?
- [ ] Are the three baseline commands green with zero warnings?

---

## Common Mistakes

| Mistake | Fix |
|---|---|
| Reporting done with `eslint` warnings outstanding | `--max-warnings 0` is part of done |
| Snapshot test accepted without reading the diff | Review the `.snap` change or delete the test |
| Testing implementation details (state, class names) | Assert user-visible behavior |
| Mocking your own hooks to test a component | Mock the HTTP boundary with MSW instead |
| Adding `eslint-disable` to get green | Fix the code, or narrow disable with a reason |
| Skipping the keyboard pass because "it's a small change" | Focus and Escape regressions ship from small changes |
| `useEffect` + `setState` to fix a render-order problem | Restructure the data flow |
| New component copied from an existing one | Extract shared primitive or add a variant |
