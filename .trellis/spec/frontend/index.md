# Frontend Development Guidelines

> React conventions for this project, built on React best practices.

---

## Overview

The frontend is a React 19 + TypeScript application. These specs encode **React best practices**
from the Vercel React / composition-pattern guides and the Callstack React Native optimization
guide, applied to a browser SPA: composition over configuration, server state separated from UI
state, hooks as the only stateful primitive, measurement before optimization, and a type system
strict enough that the compiler catches what a review would.

> **Status**: no frontend code exists yet. These files define the target conventions that
> `trellis-implement` / `trellis-check` must follow once implementation starts. When the first
> real components land, revisit each file and replace illustrative paths with real ones.

---

## Pre-Development Checklist

Before writing frontend code in a task:

- [ ] Read the relevant guide(s) below and the task's `prd.md`.
- [ ] Decide where the code lives: `features/<feature>/` vs `components/` (see
      `directory-structure.md`). Shared before it is shared twice is a mistake.
- [ ] Classify the state: server data, URL state, form state, ephemeral UI state, or global UI
      state (see `state-management.md`). Most state is not global.
- [ ] Confirm the data contract: is there a Zod schema for the response? Types must be derived from
      validation, not asserted.
- [ ] Confirm the component's accessibility contract: semantic element, label, keyboard path, focus
      behavior.
- [ ] Confirm loading, empty, error, and partial-data states — all four, not just the happy path.
- [ ] Confirm no new dependency without a reason; prefer the standard library and existing stack.
- [ ] For anything performance-sensitive: is there a measured baseline, or is this speculative?
- [ ] For a component that is growing boolean props: does it need explicit variants instead?
- [ ] If React Compiler is not enabled, does the memoization policy follow
      `performance-guidelines.md`?

---

## Guidelines Index

| Guide | Description | Status |
|-------|-------------|--------|
| [Directory Structure](./directory-structure.md) | Feature-first layout, naming, import rules | Filled |
| [Component Guidelines](./component-guidelines.md) | Composition, props, compound components, styling, a11y | Filled |
| [Hook Guidelines](./hook-guidelines.md) | Custom hooks, effect discipline, state updates, data fetching | Filled |
| [State Management](./state-management.md) | State categories, server state, derived state, stores | Filled |
| [Type Safety](./type-safety.md) | Strict TypeScript, schema-first types, forbidden casts | Filled |
| [Performance Guidelines](./performance-guidelines.md) | Measure-first, waterfalls, bundle size, re-render and rendering rules | Filled |
| [Quality Guidelines](./quality-guidelines.md) | Linting, testing, review checklist, forbidden patterns | Filled |

---

## Quality Check

Run before reporting a task complete (details in `quality-guidelines.md`):

```bash
tsc --noEmit
eslint . --max-warnings 0
vitest run
```

---

## React Rules of Thumb (quick reference)

| Topic | Rule |
|---|---|
| Components | Function components only; no class components, no `forwardRef` (React 19 passes `ref` as a prop) |
| Composition | Compound components + explicit variants over boolean-prop matrices; lift state into a provider |
| Context | `state` / `actions` / `meta` interface; read with `use()`, not `useContext()` |
| State | `useState` for local, URL for navigation, server-state library for remote, store only for cross-feature UI state |
| Updates | Functional `setState` when the next value depends on the previous; lazy init for expensive values |
| Effects | `useEffect` synchronizes with external systems only — never derives state, never models an event |
| Memoization | Conditional on whether React Compiler is enabled; no `useMemo` around primitive expressions |
| Concurrency | `startTransition` for non-urgent updates, `useDeferredValue` for expensive derived renders |
| Props | Explicit `type Props`, no `React.FC`; stable default values for memoized components |
| Types | Derived from Zod schemas; no `any`, no `as` on server data, no non-null `!` |
| Lists | Stable `key` from domain id, never array index; virtualize long lists |
| A11y | Semantic HTML first; every interactive element keyboard-reachable; dialogs always titled |
| Async UI | Every async surface has loading, error, empty, and success states |
| Bundle | No barrel imports; lazy-load heavy components; keep a size budget |

---

**Language**: All documentation is written in **English**.
