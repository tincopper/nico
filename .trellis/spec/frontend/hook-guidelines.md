# Hook Guidelines

> How hooks and data fetching are structured.

---

## Overview

Hooks are the only stateful primitive. Two kinds exist in this codebase:

1. **Data hooks** — own server state (queries/mutations) and are the only place a component talks to
   the network.
2. **Behavior hooks** — encapsulate reusable UI logic (form state, keyboard handling, derived
   values) with no network access.

A hook is justified when the logic is used twice, or when it isolates something genuinely complex
(async lifecycle, keyboard interaction, subscription). A single-use `useFoo` that wraps three lines
is indirection, not abstraction.

---

## Custom Hook Patterns

- File `use<Name>.ts` (or `.tsx` when it renders), exported as a named function `use<Name>`.
- Return an **object with named fields** when there are more than two values; a tuple only for
  genuinely positional pairs (`const [value, setValue] = useState(...)`).
- Never return JSX from a hook. Rendering belongs to components; if you need render flexibility,
  return a render-prop or accept a `children` renderer in the component instead.
- Keep a hook's state minimal: derive everything derivable at render time.
- Hooks must be safe to call unconditionally; no conditional hook calls, no hooks in loops.
- A hook that owns async work exposes an explicit state union, not loose booleans:

```ts
type AsyncState<T> =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'error'; error: Error }
  | { status: 'success'; data: T };
```

- Clean up everything you start: subscriptions, timers, `AbortController`, observers. Cleanup runs
  on unmount and on dependency change.
- `useEffect` is **only** for synchronizing with an external system (DOM measurement, browser API,
  subscription, imperative widget). It is never for deriving state, fetching on mount, or reacting
  to your own state change.
- `useLayoutEffect` only for measurements that must happen before paint; otherwise `useEffect`.
- Stable identity: an effect dependency that is an object/array recreated each render is a bug.
  Hoist it, memoize it deliberately, or restructure the hook.
- **Depend on primitive values, not objects**, so the effect re-runs only on a real transition:
  `[user.id]`, not `[user]`; derive `const isMobile = width < 768` and depend on `isMobile`, not
  `width`.
- **Split a hook whose sub-computations have different dependencies.** A combined `useMemo`
  recomputes everything when any input changes.
- **Defer a state read to its usage point** when it is only needed inside a callback — subscribing
  to `searchParams` or `localStorage` at render time causes re-renders you never use.

### State update and initialization rules

- **Use the functional `setState` form when the next value depends on the previous one.**
  `setItems((prev) => [...prev, item])` avoids a stale closure and removes `items` from the
  callback's dependency array.
- **Lazily initialize expensive state**: `useState(() => buildSearchIndex(items))`. Without the
  function form the initializer runs on every render.
- **Use `useRef` for high-frequency transient values** (pointer position, timers, transient flags)
  that must not trigger a re-render.
- **Do not model an event as state + effect.** A submit/click handler belongs in the handler, not
  in `setSubmitted(true)` followed by `useEffect(..., [submitted])`.
- **Use `useEffectEvent` for stable callback refs** (React 19.2+) instead of adding the callback to
  the dependency array — and **never put the effect event itself in the deps**, its identity
  intentionally changes every render.

```tsx
// no — re-subscribes whenever `onSearch` identity changes
useEffect(() => {
  const t = setTimeout(() => onSearch(query), 300);
  return () => clearTimeout(t);
}, [query, onSearch]);

// yes
const handleSearch = useEffectEvent(onSearch);
useEffect(() => {
  const t = setTimeout(() => handleSearch(query), 300);
  return () => clearTimeout(t);
}, [query]);   // effect event deliberately excluded
```

### Memoization policy

`useMemo`/`useCallback`/`memo` are conditional on the setup — see
[performance-guidelines.md](./performance-guidelines.md#react-compiler-is-conditional).

- **Do not wrap a simple primitive expression in `useMemo`.**
  `const isLoading = a || b` is cheaper than the hook call and its dependency comparison.
- Memoize when a profile shows wasted work, or when a dependency array needs referential stability
  across a library boundary.
- State the reason in the code or the PR when you add one.

---

## Data Fetching

Use one server-state library across the app — **TanStack Query** is the choice for this stack
(alternatives such as SWR are acceptable only if chosen once, project-wide).

Rules:

- **Components never call `fetch` directly.** Transport lives in `lib/apiClient.ts`; queries and
  mutations live in `features/<f>/api/`.
- One query key factory per feature; keys are structured arrays, never ad-hoc strings:

```ts
export const modelKeys = {
  all: ['models'] as const,
  list: (tenant: string) => [...modelKeys.all, 'list', tenant] as const,
  detail: (id: string) => [...modelKeys.all, 'detail', id] as const,
};
```

- Query functions validate the response with the feature's Zod schema and return the parsed type —
  the network boundary is where untyped data becomes typed data.
- Mutations invalidate or update the specific keys they affect; no global "refetch everything".
- Streaming (SSE) endpoints are consumed through a dedicated hook that owns the stream lifecycle,
  exposes incremental state, and aborts on unmount — not through the query cache.
- Loading/error/empty are surfaced to the component as a single `status` union (see above), so
  impossible UI states cannot be rendered.
- **Use `useTransition`/`useDeferredValue` instead of hand-rolled `isLoading` booleans** when the
  loading state exists only to keep the UI responsive during a heavy re-render. `useTransition`
  gives you `isPending` for free; `useDeferredValue` lets the input stay snappy while results
  render.
- **Independent requests run in parallel** (`Promise.all`), and a cheap synchronous guard is checked
  **before** an `await`ed flag. See
  [performance-guidelines.md](./performance-guidelines.md#critical-eliminating-waterfalls).
- **Deduplicate global listeners** when the same hook is mounted in many components: one module
  level listener with a `Set` of callbacks, not one `addEventListener` per instance.
- Scroll and resize listeners are registered with `{ passive: true }`.
- Set `staleTime`/`retry` explicitly per query when the default is wrong for the data's volatility;
  silent defaults are how flaky UIs happen.
- Do not put server data into local component state "for editing" without a deliberate dirty-state
  model; otherwise you own cache invalidation by hand.

---

## Naming Conventions

| Thing | Rule | Example |
|---|---|---|
| Hook name | `use` + PascalCase verb phrase | `useChatStream`, `useModelPicker` |
| Hook file | same name as the hook | `useChatStream.ts` |
| Query key factory | `<domain>Keys` | `chatKeys` |
| Query hook | `use<Domain>` | `useModels` |
| Mutation hook | `use<Verb><Domain>` | `useCreateApiKey` |
| Boolean returned | `is*`/`has*`/`can*` | `isStreaming`, `canRetry` |
| Handler returned | `on*` only in components; hooks return verbs | `send`, `cancel`, `reset` |
| Event subscription hook | `use<Thing>Event` | `useKeyboardShortcut` |

Hooks are camelCase; only components are PascalCase. ESLint `react-hooks/rules-of-hooks` +
`exhaustive-deps` are errors, not warnings.

---

## Common Mistakes

| Mistake | Why it hurts | Do instead |
|---|---|---|
| `useEffect` to set state derived from other state | Extra render, flicker, stale values | Compute during render |
| `useEffect` + `fetch` on mount | Duplicates the server-state library, no cache/dedupe | Query hook |
| Missing dependency in the deps array | Stale closure bugs that only appear in production | `exhaustive-deps` as error; restructure |
| Disabling `exhaustive-deps` with a comment | Hides a real dependency problem | Fix the data flow |
| `useCallback` on everything | Noise, no measurable benefit, fights the compiler when it is enabled | Memoize only with a stated reason |
| `useMemo` around a primitive expression | Hook + dependency comparison costs more than the expression | Inline it |
| `setItems([...items, item])` inside a `useCallback` | Stale closure or a dependency that recreates the callback | Functional update |
| `useState(expensiveCompute())` | Initializer runs on every render | `useState(() => expensiveCompute())` |
| Event modeled as `state` + `useEffect` | Effect re-runs on unrelated changes; duplicate side effects | Do it in the handler |
| `useEffectEvent` result in the deps array | Re-runs every render; lint error | Depend on the reactive values only |
| `[user]` object dependency | Re-runs on every unrelated field change | `[user.id]` |
| One listener per component instance for a global event | N listeners for one event | Module-level listener + callback set |
| Scroll handler without `{ passive: true }` | Blocks scrolling | Passive listener |
| Returning loose booleans from a data hook | Impossible states render (loading + error at once) | Return a `status` union |
| Hook starting a subscription/timer with no cleanup | Leaks across unmount and reload | Return cleanup; abort in-flight work |
| `useState` holding a full server response for editing | Two sources of truth | Dirty-state model or server-state library |
| Hook importing from `features/` while living in `hooks/` | Cross-feature coupling | Move it into the feature |
| `useEffect` for keyboard/scroll listeners on every render | Re-subscribes constantly | Stable handler + explicit deps |
| Fetching in a hook that a component calls multiple times | Request storms | Query dedupe by key |
