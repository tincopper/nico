# Directory Structure

> How frontend code is organized.

---

## Overview

**Feature-first, with a shared layer that earns its place.** A feature owns its components, hooks,
api calls, types, and tests. `components/` holds only primitives that are already used by two or
more features (or that are clearly generic UI: buttons, dialogs, inputs).

Rules:

1. **Colocate by feature, not by technical type.** `features/chat/` beats
   `components/chat/` + `hooks/chat/` + `api/chat/`.
2. **Promote, don't predict.** A component moves to `components/` after its second real consumer
   appears. Moving it early creates a de-facto generic API that nobody asked for.
3. **One-way imports.** `features/*` may import `components/`, `lib/`, `hooks/`, `types/`.
   `components/` and `lib/` must never import from `features/`. Cross-feature imports are forbidden —
   extract to shared instead.
4. **No barrel-file sprawl.** An `index.ts` per feature is fine as its **public** surface; deep
   `index.ts` chains that re-export everything cause cycles, slow builds, and cold-start cost. See
   Import Rules below.

---

## Directory Layout

```text
src/
├── app/                     # app shell: router, providers, layouts, error boundaries
│   ├── router.tsx
│   ├── providers.tsx
│   └── layouts/
├── features/                # one folder per product capability
│   ├── chat/
│   │   ├── components/      # feature-local components
│   │   ├── hooks/           # feature-local hooks
│   │   ├── api/             # queries/mutations + Zod schemas
│   │   ├── types.ts
│   │   ├── index.ts         # public surface of this feature
│   │   └── __tests__/
│   └── models/
├── components/              # shared, generic UI primitives (used by 2+ features)
│   ├── ui/                  # button, input, dialog, tooltip...
│   └── layout/              # page shell pieces
├── hooks/                   # shared hooks with no feature knowledge
├── lib/                     # framework-agnostic utilities: formatting, fetch client, storage
├── types/                   # shared domain types and generated API types
├── styles/                  # global CSS, design tokens
└── test/                    # test setup, fixtures, msw handlers
```

Files at feature root stay shallow: `types.ts`, `index.ts`, `constants.ts`. Anything else gets a
subfolder — no ten-file flat directories.

---

## Module Organization

| Kind of code | Location | Notes |
|---|---|---|
| Page / route entry | `features/<feature>/components/<Name>Page.tsx` or `app/` when it is pure routing | Pages compose; they do not implement logic |
| Reusable UI primitive | `components/ui/` | No feature imports, no data fetching |
| Feature component | `features/<f>/components/` | May fetch, may use feature hooks |
| Data fetching | `features/<f>/api/` | Queries, mutations, Zod schemas together |
| Cross-feature hook | `hooks/` | Must not import from `features/` |
| Pure helper | `lib/` | No React imports at all |
| Global provider/store setup | `app/providers.tsx` | One place, explicit order |

A file that needs both `react` and domain types belongs to a feature, not to `lib/`.

---

## Naming Conventions

| Thing | Rule | Example |
|---|---|---|
| Component file | `PascalCase.tsx`, one primary component | `ModelPicker.tsx` |
| Component name | `PascalCase`, noun | `ModelPicker` |
| Hook file | `use<PascalCase>.ts`, camelCase hook | `useModelPicker.ts` |
| Hook name | `useX` | `useModelPicker` |
| Non-component module | `camelCase.ts` | `formatTokens.ts` |
| Type file | `types.ts` or `<domain>.types.ts` | `chat.types.ts` |
| Test file | `<name>.test.tsx` beside the source or in `__tests__/` | `ModelPicker.test.tsx` |
| Constant | `SCREAMING_SNAKE_CASE` for true constants, `camelCase` for config objects | `MAX_ATTACHMENTS` |
| Boolean prop | `is*` / `has*` / `can*` | `isLoading`, `hasError` |
| Event prop | `on<Event>` | `onSelect`, `onRetry` |
| Feature folder | kebab-case | `features/api-keys/` |

Component files export exactly one component as default or named (pick one per codebase; prefer
**named exports** so rename refactors are reliable). No file exports a component *and* unrelated
helpers.

---

## Import Rules

```ts
// allowed
import { Button } from '@/components/ui/Button';
import { useModels } from '@/hooks/useModels';
import { formatTokens } from '@/lib/formatTokens';
import { useChatMessages } from '../hooks/useChatMessages';

// forbidden
import { ChatPanel } from '@/features/chat';        // from another feature
import { useChat } from '@/features/chat/hooks/...'; // reaching into feature internals
import { Button } from '../../../components/ui/Button'; // deep relative climb
```

- Use the `@/` path alias for anything outside the current folder; `./`/`../` only within a feature.
- Enforce with ESLint (`no-restricted-imports` or `import/no-restricted-paths`) rather than review
  discipline.
- No circular imports, including through `index.ts` barrels. If a cycle appears, the shared piece is
  in the wrong layer.

---

## Examples

- New screen: create `features/<name>/` with `components/<Name>Page.tsx`, `api/`, `hooks/`,
  `types.ts`, `index.ts`. Register the route in `app/router.tsx`.
- Second consumer for a component: move it from `features/<a>/components/` to `components/ui/`,
  strip feature-specific props, add a story or test.
- New API endpoint: add the Zod schema + query/mutation in `features/<name>/api/`, derive the types
  from the schema, and consume it through a feature hook.
