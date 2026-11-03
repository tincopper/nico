# Component Guidelines

> How components are written in this project.

---

## Overview

Components are **function components that render UI and nothing else**. They do not fetch data
directly, do not hold cross-feature state, and do not branch on configuration where a variant
belongs. Behavior comes from props and hooks; composition comes from `children` and explicit slots.

React 19 baseline: no class components, no `forwardRef` (ref is a normal prop), no legacy
`defaultProps`, `use` for promises/context where it simplifies, Actions for form submission.

---

## Component Structure

Order inside a component file:

1. Imports (types first, then values, then styles).
2. Types (`type Props = ...`, local variant unions).
3. Module-level constants (outside the component).
4. The component function.
5. Small local subcomponents (only if used once; otherwise extract to a file).
6. No exports other than the component.

```tsx
import type { Model } from '@/types/model';

type Props = {
  models: readonly Model[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  disabled?: boolean;
};

export function ModelPicker({ models, selectedId, onSelect, disabled = false }: Props) {
  if (models.length === 0) {
    return <EmptyState message="No models available" />;
  }

  return (
    <ul role="listbox" aria-label="Models" className="...">
      {models.map((model) => (
        <li key={model.id} role="option" aria-selected={model.id === selectedId}>
          <button type="button" onClick={() => onSelect(model.id)} disabled={disabled}>
            {model.displayName}
          </button>
        </li>
      ))}
    </ul>
  );
}
```

Rules:

- Destructure props in the signature; default values in the signature, not with `defaultProps`.
- Keep a component under ~120 lines. Beyond that, extract subcomponents or move logic into hooks.
- Early returns for empty/error/loading states before the main render — no deeply nested ternaries.
- No `useEffect` that computes a value render could compute (see `hook-guidelines.md`).
- Do not create components inside another component's body (they remount every render).
- Module-level constants and stable objects stay outside the component so they are not recreated.

---

## Props Conventions

- Always an explicit `type Props`, exported when a parent needs it: `export type Props = {...}`.
- Do **not** use `React.FC`, `React.FunctionComponent`, or `JSX.Element` return annotations — let
  inference do the work.
- `children` is typed `React.ReactNode` and is a real composition point, not a dumping ground.
- Prefer **discriminated unions over boolean matrices**:

```tsx
// preferred
type Props =
  | { status: 'idle' }
  | { status: 'loading' }
  | { status: 'error'; error: string; onRetry: () => void }
  | { status: 'success'; data: ChatMessage[] };

// avoid — allows impossible combinations
type BadProps = {
  isLoading: boolean;
  isError: boolean;
  isSuccess: boolean;
  data?: ChatMessage[];
  error?: string;
};
```

- Boolean props are allowed only for genuine binary toggles (`disabled`, `required`). Two booleans
  that interact (`isCompact` + `isExpanded`) become one `variant` prop.
- Callback props are named `on<Event>` and typed with the narrowest argument the child actually
  knows (`(id: string) => void`, not `(e: MouseEvent) => void`, unless the DOM event is needed).
- No prop drilling beyond two levels — either compose with `children`, or introduce a feature-level
  context (see Composition Patterns below).
- Spread props (`{...rest}`) only on UI primitives forwarding native attributes; never to pass
  business data through a component.
- Optional props default in the signature; never mutate props, never store them in state.
- **Default values must be stable** when the component is memoized: a default `[]`, `{}` or `() => {}`
  creates a new instance every render and breaks `memo()`'s equality check. Hoist it to a
  module-level constant:

```tsx
// no — new function each render, memo never hits
const Item = memo(function Item({ onSelect = () => {} }: Props) { ... });

// yes
const NOOP = () => {};
const Item = memo(function Item({ onSelect = NOOP }: Props) { ... });
```

- React 19: `ref` is a normal prop. Type it as `ref?: React.Ref<T>` in `Props` and pass it
  through — no `forwardRef` wrapper.

---

## Composition Patterns

Composition beats configuration. These patterns are the reference for any component that starts
accumulating boolean props.

### Boolean props are a design smell

Each boolean doubles the possible states and multiplies the conditional branches. When a component
reaches three related booleans, stop and restructure.

```tsx
// no — 3 booleans = 8 states, most of them meaningless
type Props = {
  isThread: boolean; isEditing: boolean; isForwarding: boolean;
  channelId?: string; dmId?: string; messageId?: string;
};

// yes — explicit variants that compose shared parts
export function ThreadComposer({ channelId }: { channelId: string }) {
  return (
    <Composer.Frame>
      <Composer.Header />
      <Composer.Input />
      <AlsoSendToChannelField id={channelId} />
      <Composer.Footer><Composer.Submit /></Composer.Footer>
    </Composer.Frame>
  );
}

export function EditComposer({ messageId }: { messageId: string }) {
  return (
    <Composer.Frame>
      <Composer.Input />
      <Composer.Footer><Composer.CancelEdit /><Composer.SaveEdit /></Composer.Footer>
    </Composer.Frame>
  );
}
```

Each variant states what it renders. No boolean combination to reason about, no impossible states.

### Compound components with shared context

Expose a namespace object (`Composer.Frame`, `Composer.Input`, `Composer.Footer`) rather than one
monolithic component. Internals stay private; the composition is the public API.

### Lift state into a provider, not into props

When a sibling outside the visual subtree needs the state (a preview pane, a submit button in a
dialog footer), do **not** thread it through props, sync it with `useEffect`, or pass a `stateRef`.
Put it in a provider component that wraps both.

```tsx
// no — effect syncing state upward, or a ref read on submit
<Composer onInputChange={setInput} />
<Preview input={input} />

// yes — provider owns the state, both siblings read it
<ComposerProvider>
  <Composer.Frame><Composer.Input /></Composer.Frame>
  <Preview />            {/* outside Composer.Frame, inside the provider */}
  <SubmitButton />       {/* calls actions.submit from context */}
</ComposerProvider>
```

Components that need shared state do not have to be visually nested — they only need to be inside
the same provider.

### Context interface: `state` / `actions` / `meta`

Define a **generic** context contract so the same UI works with different state implementations
(local `useState`, a global store, or a server-synced source):

```ts
type ComposerContextValue = {
  state: { input: string; attachments: Attachment[]; isSubmitting: boolean };
  actions: { update: (updater: (s: State) => State) => void; submit: () => void };
  meta: { inputRef: React.RefObject<HTMLTextAreaElement | null> };
};
```

Rules:

- **The provider is the only place that knows how state is managed.** UI components consume the
  interface and never import a specific store or hook.
- Read the context with `use(ComposerContext)` in React 19, not `useContext()`.
- `meta` carries non-reactive handles (refs), `actions` carries behavior, `state` carries data.
- Swapping the provider must not require touching any UI component.

### `children` over render props

Use `children` for static structure composition. Render props are appropriate only when the parent
must hand data back to the child — e.g. a virtualized list's `renderItem`.

```tsx
// no — renderHeader/renderFooter/renderActions
<Composer renderHeader={() => <Header />} renderFooter={() => <Footer />} />

// yes
<Composer.Frame>
  <CustomHeader />
  <Composer.Input />
  <Composer.Footer><Composer.Submit /></Composer.Footer>
</Composer.Frame>
```

---

## Styling Patterns

- **One styling system per codebase.** Choose Tailwind CSS (preferred for this stack) or CSS Modules
  and do not mix utility classes with ad-hoc `style={{}}` objects.
- Design tokens (color, spacing, radius, typography) live in `styles/` and are referenced by
  semantic name; raw hex values and magic pixel numbers do not appear in components.
- Conditional classes go through a single helper (`clsx` / `cn`), never string concatenation with
  ternaries scattered across JSX.
- Inline `style` is reserved for genuinely dynamic values (measured dimensions, progress width).
- Component-level CSS files are allowed for complex animations; global selectors and element
  selectors (`div > span`) are not.
- Dark mode / themes are driven by tokens and a single root attribute, not per-component conditionals.
- **Use semantic tokens, never raw palette values**: `bg-background`, `text-muted-foreground`,
  `bg-primary`. `bg-blue-500` is a finding, not a style choice. Manual `dark:` overrides on top of
  semantic tokens are also a finding.
- **`className` is for layout, not for overriding a component's colors or typography.**
- Prefer `gap-*` with `flex` over `space-x-*`/`space-y-*`; use `size-*` when width and height are
  equal; use `truncate` instead of `overflow-hidden text-ellipsis whitespace-nowrap`.
- Do not set manual `z-index` on overlays (dialog, sheet, popover) — the primitive manages its own
  stacking.

---

## Accessibility

Accessibility is part of "done", not a follow-up task.

- **Semantic HTML first.** `<button>`, `<a>`, `<label>`, `<dialog>`, `<ul>/<li>`, `<nav>`, `<main>`.
  ARIA is a fallback when no element expresses the role, not decoration. "No ARIA is better than
  bad ARIA."
- Every interactive element is reachable and operable by keyboard: focusable, visible focus ring,
  Enter/Space activation, Escape to dismiss overlays.
- Every input has a programmatic label (`<label htmlFor>` or `aria-label`), not placeholder-only.
- Images: `alt` describing purpose, `alt=""` when decorative. Icons-only buttons require
  `aria-label`.
- Do not remove focus outlines without providing an equivalent visible focus indicator.
- Modals: trap focus, restore focus on close, `aria-modal`, labelled by a heading; use
  `<dialog>`/Radix-style primitives rather than a hand-rolled div with `role="dialog"`. A dialog,
  sheet, or drawer **always** needs a visible or `sr-only` title.
- Popovers/dropdowns: items belong inside their group (`SelectItem` inside `SelectGroup`,
  `DropdownMenuItem` inside `DropdownMenuGroup`) — grouping is both structure and screen-reader
  semantics.
- Announce async results that are not reflected by a focus change via an `aria-live` region
  (polite for updates, assertive for errors).
- Never encode state by color alone — pair with text, icon, or pattern.
- Test with keyboard only and with a screen reader before considering a component finished; see the
  `screen-reader-testing` and `wcag-audit-patterns` skills.
- Target WCAG 2.2 AA: contrast ≥ 4.5:1 for text, 3:1 for large text and UI boundaries; hit target
  ≥ 24×24 CSS px.

---

## Common Mistakes

| Mistake | Why it hurts | Do instead |
|---|---|---|
| `React.FC<Props>` | Adds implicit `children` typing and blocks generics | Plain function with `type Props` |
| Boolean-prop matrix (`isLoading`/`isError`/`isSuccess`) | Impossible states become representable | Discriminated union on `status` |
| Component defined inside another component | Remounts every render, loses state and focus | Module-level component |
| `style={{ marginTop: 13 }}` | Breaks token system and theming | Token-backed class |
| `onClick` on a `<div>` | Keyboard and screen-reader users cannot activate it | `<button>` or `<a>` |
| Placeholder as the only label | Invisible to screen readers, disappears on input | `<label>` / `aria-label` |
| Rendering a list with `key={index}` | Wrong row state after reorder/filter | Stable domain id |
| Fetching inside a presentational component | Couples UI to transport, untestable | Pass data via props / feature hook |
| Two components in one file with shared local state via closure | Hidden coupling | One component, or explicit shared hook |
| `dangerouslySetInnerHTML` with server/user content | XSS | Render text, or sanitize through a vetted library |
| Copy-pasting a component and tweaking it | Divergence and double maintenance | Add a variant prop or compose |
| Three interacting boolean props | 2^n impossible states | Explicit variant components |
| Sibling state synced via `useEffect` or a ref | Fragile ordering, missed updates | Lift into a provider |
| `{count && <Badge />}` with `count = 0` | Renders a literal `0` | `{count > 0 ? <Badge /> : null}` |
| Dialog without a title | Screen readers announce nothing on open | Visible or `sr-only` title |
| `bg-blue-500` / manual `dark:` overrides | Breaks theming and dark mode | Semantic token (`bg-primary`) |
| `space-x-4` between items | Margin-based spacing interacts badly with wrap/removal | `flex gap-4` |
| Unmounting a toggled expensive subtree | Re-mount cost and lost state on every toggle | `<Activity mode="hidden">` |
| Non-virtualized long list | Every row mounts, scroll jank | Virtualized list, or `content-visibility: auto` |
| `memo()` with a default `= []` / `= () => {}` | New instance each render, memo never hits | Hoist the default to a constant |
