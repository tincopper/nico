# Type Safety

> TypeScript conventions and validation boundaries.

---

## Overview

Two rules drive everything here:

1. **`strict` TypeScript, no escape hatches.** The compiler is the first reviewer; if a cast is
   needed to make something compile, the design is wrong.
2. **Untyped data exists only at the boundary.** Network payloads, storage, URL params, and
   environment values are `unknown` until a schema validates them. Types are *derived* from the
   schema, so validation and types can never disagree.

---

## Type Organization

| Kind of type | Location |
|---|---|
| Domain entity shared across features | `src/types/<domain>.ts` |
| Feature-local types (props, view models) | `features/<f>/types.ts` |
| API response/request types | Derived from the Zod schema in `features/<f>/api/` |
| Generated types (OpenAPI/GraphQL) | `src/types/generated/` — never hand-edited |
| Component prop types | In the component file, exported when reused |

Rules:

- `type` for unions, intersections, and object shapes; `interface` only when declaration merging is
  genuinely needed (it rarely is). Pick `type` as the default.
- Prefer `readonly` for data that must not be mutated: `readonly Model[]`, `readonly` fields.
- Use discriminated unions for state machines and variants instead of optional-field soup.
- Use `satisfies` to keep literal inference while checking shape:
  `const config = { retries: 2, timeout: 5000 } satisfies RetryConfig;`
- No enums. Use `as const` objects plus a derived union:

```ts
export const Role = { Admin: 'admin', Member: 'member' } as const;
export type Role = (typeof Role)[keyof typeof Role];
```

- Branded types for identifiers that must not be interchangeable:
  `type ModelId = string & { readonly __brand: 'ModelId' };`
- Utility types (`Pick`, `Omit`, `Partial`) are fine at the API edge; do not use them to paper over
  a missing domain type.

---

## Validation

**Zod at every boundary**, one schema per payload, colocated with the API call.

```ts
// features/chat/api/schemas.ts
export const chatMessageSchema = z.object({
  id: z.string(),
  role: z.enum(['user', 'assistant', 'system']),
  content: z.string(),
  createdAt: z.string().datetime(),
  usage: z.object({ inputTokens: z.number().int().nonnegative() }).optional(),
});
export type ChatMessage = z.infer<typeof chatMessageSchema>;
```

Rules:

- Derive types with `z.infer` — never write a parallel hand-maintained interface.
- Parse in the query/mutation function, not in the component:
  `return chatMessageSchema.array().parse(await response.json());`
- Objects are `.strict()` at trust boundaries so unknown fields are visible rather than silently
  dropped, unless forward-compatibility requires pass-through — then document it.
- External IDs (route params, form input) are validated too, with a coercing schema where
  appropriate.
- On validation failure, throw a typed `ApiError` with the issue list; the UI shows a generic
  message and the details go to the error reporter.
- Never validate "sometimes". A schema that is only applied on one code path is a latent bug.

---

## Common Patterns

- **Exhaustiveness checking** on unions:

```ts
switch (state.status) {
  case 'idle': return <Idle />;
  case 'loading': return <Spinner />;
  case 'error': return <ErrorState error={state.error} />;
  case 'success': return <Content data={state.data} />;
  default: {
    const _exhaustive: never = state;
    return _exhaustive;
  }
}
```

- **Type guards** are rare because Zod covers parsing; when needed, they must be predicates with a
  real runtime check, not a cast in disguise.
- **Generics** for reusable data structures; if a component needs a generic, verify the type is
  actually carried through every branch.
- **Narrow at the edge**: convert `string | undefined` from the URL into a domain type once, in one
  place, instead of `!` everywhere downstream.
- **Return types on public functions** — exported functions and hooks declare their return type so
  refactors cannot silently change the contract.

---

## Forbidden Patterns

| Pattern | Why | Instead |
|---|---|---|
| `any` | Disables the type system silently | `unknown` + schema parse, or a real type |
| `as` on server data | Compile-time lie about runtime shape | Zod `.parse()` |
| `as unknown as T` | Double cast, hides a real mismatch | Fix the types; restructure |
| Non-null assertion `!` | Crashes at runtime when the assumption fails | Explicit check / narrow at the edge |
| `@ts-ignore` | Silently suppresses the next line forever | `@ts-expect-error` with a reason, or fix it |
| `@ts-expect-error` without a comment | No one knows why it exists | Always pair with the reason and a link |
| `// eslint-disable` for type rules | Same | Fix the type |
| `Function` / `object` as types | Meaningless | Precise signature / `Record<string, unknown>` |
| Optional field used to model a variant | Impossible states | Discriminated union |
| `interface` extending to model a union | Impossible to model properly | `type` union |
| `JSON.parse()` result used directly | Untyped data | Schema parse |
| Enum | Runtime object, poor interop | `as const` + derived union |
| Untyped `Record<string, unknown>` spread into props | Leaks unknown shape | Explicit fields |
| Ignoring `exactOptionalPropertyTypes` / `noUncheckedIndexedAccess` errors | Null holes at runtime | Handle the empty case |

`tsconfig.json` must include `strict: true`, `noUncheckedIndexedAccess`,
`exactOptionalPropertyTypes`, `noImplicitOverride`, `noFallthroughCasesInSwitch`,
`verbatimModuleSyntax`, `isolatedModules`. Downgrading any of these is a design decision requiring
a spec update, not a local tweak.
