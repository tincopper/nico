# State Management

> How state is classified and where it lives.

---

## Overview

**Most state should not be global.** The governing rule is to classify state by *who owns it* and
*what its lifecycle is*, then use the smallest mechanism that fits. A global store is the last
resort, not the default.

Four categories, in order of preference:

| Category | Owner | Mechanism |
|---|---|---|
| Derived | Nobody | Compute during render |
| Local UI | One component (or one component tree) | `useState` / `useReducer` |
| Server | The backend | TanStack Query cache |
| URL | Navigation semantics | Router state, search params |
| Global UI | Cross-feature app concern | A single store (Zustand) or context |

---

## State Categories

### Derived state — do not store it

If a value can be computed from existing props/state, compute it. Storing it means keeping two
things in sync forever.

```tsx
// no
const [filtered, setFiltered] = useState<Model[]>([]);
useEffect(() => setFiltered(models.filter((m) => m.active)), [models]);

// yes
const filtered = models.filter((m) => m.active);
```

Memoize only when the computation is measurably expensive.

### Local UI state

`useState` for a single primitive or a small flat object; `useReducer` when transitions are
multi-field and must stay consistent (e.g. a form wizard, a streaming chat turn).

Keep state as low in the tree as possible. If two siblings need the same state, lift it to their
common parent — not to a global store.

When a sibling **outside** the visual subtree needs it (a preview pane, a submit button in a dialog
footer), lift it into a **provider component** that wraps both. Do not thread it through props,
sync it upward with `useEffect`, or read it from a `stateRef` on submit — all three are fragile.
Components that need shared state do not have to be visually nested; they only need to be inside the
same provider. See
[component-guidelines.md](./component-guidelines.md#composition-patterns).

Ephemeral, high-frequency values (pointer position, scroll offset, transient flags) belong in a
`ref`, not in state — updating a ref does not re-render.

### Server state

Anything the backend owns: lists, entities, pagination, mutations. It lives in the query cache
(TanStack Query) with structured keys, and is never copied into local state or a global store.

- Server data is read through query hooks; components do not hold it.
- Invalidation is scoped to affected keys.
- Optimistic updates are allowed only with a defined rollback path.
- Streaming responses (SSE) are local hook state, not query cache entries.

### URL state

Anything that defines "which view am I looking at": route params, filters, sort, pagination,
selected tab that must survive refresh or be shareable. URL state is the source of truth for
navigation — do not mirror it into a store.

### Global UI state

Only genuinely cross-feature concerns: theme, locale, active workspace/tenant, global modals,
auth session summary, feature flags. One store, small and explicit.

---

## When to Use Global State

Promote state to a global store only when **all** of these hold:

1. Two or more features read or write it.
2. It has no server-side source of truth (or the server copy is a cache).
3. It must survive route changes.
4. Passing it via props or context would require drilling through 3+ layers.

If any is false, keep it local, derive it, or put it in the URL.

Store rules:

- **Zustand** is the store for this project: one store per concern (`useUiStore`,
  `useSessionStore`), not one god store.
- Actions live in the store; components call actions, they do not `set` partial state directly.
- No fetching inside a store. No React component types inside a store.
- Select narrowly (`useUiStore((s) => s.theme)`) so unrelated updates do not rerender consumers.
- Do not use context for frequently changing data — context is for stable dependencies
  (theme, i18n, feature flags), not per-keystroke state.
- No two-way sync between store and query cache. Pick one owner per piece of data.

---

## Server State

- One query cache instance, created in `app/providers.tsx`, configured once (retry policy,
  `staleTime` defaults, error normalization).
- Keys are namespaced arrays from a per-feature factory (see `hook-guidelines.md`).
- Response parsing and validation happen in the query function, so the cache only ever holds typed,
  validated data.
- Mutations return the updated entity when the API supports it and write it into the cache, rather
  than always refetching the world.
- Errors are normalized into a single `ApiError` shape at the client boundary so UI code has one
  error contract.
- Cache lifetime is bounded: no `staleTime: Infinity` for volatile data; no unbounded `gcTime` for
  large collections.

---

## Common Mistakes

| Mistake | Why it hurts | Do instead |
|---|---|---|
| Copying server data into `useState` | Two sources of truth, stale after refetch | Read from the query hook |
| Global store for a single component's state | Indirection, no benefit, hard to delete later | `useState` |
| Mirroring URL params into a store | Desync on back/forward | Read from the router |
| `useEffect` syncing store ↔ query cache | Feedback loops, lost updates | One owner |
| Selecting the whole store (`useStore()`) | Rerenders on every change | Selector per field |
| Context for high-frequency state | Rerenders the whole subtree | Local state or a store selector |
| Deriving state in an effect | Flicker and extra renders | Compute during render |
| Storing `Date`/`Map`/`Set` in a serialized store | Breaks persistence/devtools | Store primitives, convert at the edge |
| Optimistic update with no rollback | UI lies on failure | Define `onError` rollback explicitly |
| One god store with 20 fields | Everything couples | One store per concern |
| Keeping auth tokens in a store and localStorage | XSS-exposed and unsynchronized | Follow the auth model defined in the security spec |
| `useEffect` syncing state upward to a parent | Re-runs on every change, ordering bugs | Lift into a provider |
| `stateRef` read on submit to reach a sibling | Silent stale reads, no reactivity | Provider context |
| High-frequency values in `useState` | Re-renders on every pointer/scroll event | `useRef`, or `startTransition` if the UI must reflect it |
| `localStorage.setItem(key, JSON.stringify(fullObject))` | Schema conflicts, oversized/sensitive payloads, throws in private mode | Versioned key (`config:v2`) + minimal fields + `try/catch` |
