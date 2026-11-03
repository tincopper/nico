# Performance Guidelines

> Measure first, optimize second. Frontend performance rules for this project.

---

## Overview

Two governing rules, both taken from the Vercel React and React Native best-practice guides:

1. **Measure → Optimize → Re-measure → Validate.** Never recommend or apply memoization, atomic
   state, a compiler, or a bundle change without a measured render, FPS, or size problem.
2. **Do not report a problem speculatively.** No stale-closure claims without a repro path, no
   "this looks expensive" without a profile.

If a change does not move the metric, revert it. An optimization without a number is a
complication.

> **Status**: no frontend code exists yet. The tooling below is the target setup; run the
> measurement steps once a real app exists.

---

## Measurement Tooling

| What | How |
|---|---|
| Component re-renders | React DevTools Profiler — record the actual interaction, read commit duration + re-render counts |
| FPS / frame drops | Performance panel or a frame-timing harness; record the interaction, not just the page |
| Bundle composition | `npx source-map-explorer dist/assets/*.js` (or the Vite/Next equivalent) |
| Bundle size | record baseline, change, re-bundle, compare (e.g. 2.1 MB → 1.6 MB) |
| Startup / TTI | only cold starts count; warm/hot/prewarm numbers are noise |

Rules:

- Profile the **target interaction**, not the component tree depth or component count. Tree depth
  is context, not evidence.
- Check library versions before flagging an API-level fix. Example: FlashList v2 deprecates
  `estimatedItemSize` — do not report it as missing there.
- Only change `useMemo`/`useCallback` dependencies when behavior is demonstrably incorrect or a
  profile shows wasted work tied to that value.

---

## React Compiler Is Conditional

Whether the compiler is enabled decides the memoization policy. Verify the installed setup
(`babel-plugin-react-compiler` / `eslint-plugin-react-hooks` compiler rules) before applying
either branch:

**If React Compiler is enabled**

- Do not add `useMemo`/`useCallback` for routine memoization; the compiler handles it.
- Still add them when a dependency array needs referential stability across a library boundary, or
  when a profile shows the compiler did not cover the case.
- Keep components compiler-friendly: no mutating props/state, no conditionally-called hooks.

**If React Compiler is not enabled**

- Memoize where a profile shows wasted work: heavy child subtrees, expensive derived computations,
  and callbacks passed to memoized children.
- Do not blanket-memoize. `useMemo` on a primitive expression is slower than the expression
  (see `hook-guidelines.md`).

Never write a rule in code review as "the compiler will fix it" unless the compiler is actually
installed.

---

## Critical: Eliminating Waterfalls

Sequential awaits are the most common self-inflicted latency bug.

- **Independent operations run in parallel**: `Promise.all([a(), b(), c()])` instead of three
  sequential awaits.
- **Start promises early, await late**: kick off the request, do the synchronous work, then await.
- **Check cheap synchronous conditions before an async flag**:

```ts
// no: pays for the remote call even when it can never be used
const flag = await getFlag()
if (flag && isEnabled) { ... }

// yes
if (isEnabled) {
  const flag = await getFlag()
  if (flag) { ... }
}
```

- **Defer the await into the branch that uses it**, rather than awaiting a value that only one
  branch consumes.
- In a server/RSC context, parallelize per-item fetches (`Promise.all` over items) rather than
  chaining inside a loop.

---

## Critical: Bundle Size

- **No barrel imports.** Import from the source module, not from a package entry point that
  re-exports thousands of modules. Icon and component libraries can cost 200–800 ms of import time
  and hundreds of KB. See `directory-structure.md`.
- **Lazy-load heavy components** that are not needed on first render (editors, charts, syntax
  highlighters) via dynamic `import()` — in Next.js use `next/dynamic`, in Vite use
  `React.lazy` + `<Suspense>`.
- **Load third-party analytics/logging after the main content**, not in the critical path.
- **Load large data/modules only when a feature activates** (`bundle-conditional`).
- **Preload on hover/focus** for perceived speed on likely-next navigation.
- **Keep a size budget.** Record the baseline, and treat a regression beyond the agreed threshold
  as a review finding.
- Evaluate tree-shaking only with evidence; do not add a bundler plugin for speculative gains.

---

## Medium: Re-render Discipline

- Derive state during render; never via an effect (see `hook-guidelines.md`).
- Put interaction logic in event handlers, not in state + effect.
- Subscribe to derived booleans, not continuous values (`isMobile`, not `windowWidth`).
- Use `startTransition` for frequent non-urgent updates (scroll position, filter changes).
- Use `useDeferredValue` for expensive derived renders so input stays responsive.
- Use `useRef` for high-frequency transient values that must not re-render.
- Split hooks whose sub-computations have different dependencies.
- Defer state reads (search params, storage) to the callback that needs them.
- Use `<Activity mode="hidden">` instead of unmounting an expensive subtree that toggles visibility.
- Use `useTransition` for loading states instead of hand-rolled `isLoading` booleans.

---

## Medium: Rendering Performance

- **Long lists**: virtualize (see `component-guidelines.md`), or apply
  `content-visibility: auto` + `contain-intrinsic-size` to defer off-screen layout/paint.
- **Hoist static JSX** out of components — large static SVG trees are expensive to recreate.
- **Avoid defining components inside components** (remounts every render, loses state and focus).
- **Explicit conditional rendering**: use a ternary, not `&&`, when the left side can be `0` or
  `NaN`.

---

## JavaScript-Level

Low-priority, apply only inside measured hot paths:

- Build a `Map` for repeated `.find()` lookups by the same key.
- Combine multiple `filter`/`map` passes into one loop.
- Hoist `RegExp` creation out of loops.
- Cache repeated `localStorage`/`sessionStorage` reads.
- Prefer early returns over nested conditionals.
- Defer non-critical work to idle time.
- Use `toSorted()` for immutable sorts.

---

## Review Checklist

- [ ] Is there a measured baseline, or is this a speculative optimization?
- [ ] Any sequential awaits that could be `Promise.all`?
- [ ] Any new barrel import or heavy top-level import?
- [ ] Does the memoization decision match whether React Compiler is actually enabled?
- [ ] Any `useEffect` computing derived state or modeling an event?
- [ ] Any `useMemo` wrapping a primitive expression?
- [ ] Long list virtualized or `content-visibility` applied?
- [ ] Bundle size re-measured and within budget?
- [ ] After the change, is the metric better? If not, was it reverted?

---

## Common Mistakes

| Mistake | Fix |
|---|---|
| "React Compiler handles it" without checking it is installed | Verify the plugin; otherwise memoize by profile |
| Optimizing without a baseline | Measure, then change, then re-measure |
| Three sequential awaits for independent data | `Promise.all` |
| Flagging `estimatedItemSize` on FlashList v2 | Check the library version first |
| Reporting a stale closure without a repro | Show the read path, a repro, or profiler evidence |
| Blanket `useMemo` on primitives | Inline the expression |
| Treating component count/depth as the perf evidence | Profile the target interaction |
| Accepting a bundle regression "for DX" | Enforce a size budget |
| Keeping an optimization that did not move the metric | Revert it |
