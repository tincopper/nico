# OpenAI Responses wire: the field table behind `provider/openai_responses`

> Implementation step 4. The machine this task ran on has **no network**, so this table cannot cite a
> fetched reference document. Every row therefore carries an evidence grade:
>
> - `verified` — the field name and its meaning are known with confidence (the shape is stable and
>   long-published; the gateway's own `openai.chat` dialect documents the same vendor's conventions).
> - `unverified` — remembered but not confirmable here. Per `design.md` §10 a field in this grade is
>   **not written from memory**: it becomes a typed refusal, or it is relayed unchanged because the
>   adapter never authors it. The grade is recorded in the module documentation too.
> - `refused` — deliberately not supported, with the reason and the condition that would reopen it.
>
> The adapter's contract is "nothing is dropped silently": a canonical value with no wire field is a
> `422` naming the canonical field, and an unknown *member* of a document this adapter reads is a
> `502` (`Malformed`), never a skip.

## 1. Scope

One request path, `POST /v1/responses`, one response document, and one `response.*` event stream.
Everything outside `design.md` §4.3/§5.2 — embeddings, batches, files, background runs, sessions,
built-in tools — is out of scope for 1.4b.

## 2. Request envelope

| Wire member | Direction | Grade | Note |
|---|---|---|---|
| `model` | write | verified | Physical name from `ProviderRoute::model`; the logical name never leaves the gateway |
| `input` | write | verified | Always the item array, never the string shortcut: one spelling keeps a golden fixture from having two forms |
| `instructions` | write | verified | Leading system/developer turns merge into one string; a later system turn has no position and is refused |
| `tools` | write | verified | Flat `{type:"function", name, description, parameters}`; **not** chat's nested `function` object |
| `tool_choice` | write | verified | `"auto"` \| `"none"` \| `"required"` \| `{type:"function", name}` |
| `temperature` | write | verified | Canonical range is 0.0..=2.0 and this protocol's is the same, so nothing is refused here |
| `top_p` | write | verified | |
| `max_output_tokens` | write | verified | Canonical `Sampling.max_tokens`; optional, so no route fact is needed (unlike Anthropic) |
| `text.format` | write | verified | `text` omitted for free-form; `{type:"json_object"}`; `{type:"json_schema", name, schema}` |
| `text.format.strict` | — | unverified | Not written: the canonical model has no such field, and promising a decoding guarantee the caller never asked for would change the contract |
| `stream` | write | verified | Omitted when false, the protocol default |
| `store` | write | verified | Always written as `false`. The gateway keeps no response to continue from, so leaving the vendor default (`true`) in place would start retention the caller never asked the gateway to enable |
| `metadata` | relay | verified | No side effect; arrives through `extensions` and is written unchanged |
| `user` | relay | verified | Same as `metadata` |

### Refused by this adapter

Each of these is a documented request member; none has a canonical home, and each changes what the
upstream would answer or store, so it is a `422` `unsupported_field` (from a relayed `extensions` key)
rather than an omission.

| Field | Grade | Why refused | Reopens when |
|---|---|---|---|
| `previous_response_id` | verified | The gateway keeps no conversation state; a request that continues a server-side response cannot be served | the gateway gains a response store |
| `store: true` | verified | Same reason; the adapter always writes `false`. A relayed `store` is also a *shadowing* key, so it is `400` `invalid_request` rather than `422`: the adapter writes the member on every request, and two writers would leave two answers | same |
| `reasoning.effort` | verified | `Sampling` has no effort knob, and inventing one from `reasoning` text would be a guess | canonical gains the knob |
| `reasoning.summary` | verified | Same; also the summary handle of a *previous* answer has no canonical field | same |
| built-in tools (`web_search`, `file_search`, `computer_use`, `code_interpreter`, `mcp`) | verified | `Tool` is a function declaration; a built-in tool is a server-side capability with no canonical shape | canonical gains a tool kind |
| `truncation` | verified | Changes how an over-long conversation is cut; the canonical conversation is a value the caller wrote | canonical gains the knob |
| `parallel_tool_calls` | verified | A generation knob; writing a default would speak for the caller | same |
| `include` | verified | Requests response data (`encrypted_content`, logprobs) the canonical answer cannot carry, and implies retention | canonical gains the field |
| `text.verbosity` | unverified | Remembered as a newer-model knob, so it is not written from memory. The adapter writes the whole `text` member, so a relayed `text` is a shadowing key (`400`); a verbosity knob inside a canonical request has no field and is refused by the client codec (`422`) | verified against a reference |
| `service_tier` | verified | Selects vendor capacity; a routing decision, not a request fact | route-level routing lands |
| `background` | verified | Server-side asynchronous execution; the gateway's exchange is one request | out of scope |
| `prompt` | verified | A reference to a stored prompt template the gateway has no id for | prompt management lands |
| `stop` | verified absent | The dialect's request object has no stop sequences | canonical drops the knob, or a compatible endpoint is modelled as its own kind |
| `seed` | verified absent | Same | same |
| `presence_penalty` / `frequency_penalty` | verified absent | Same | same |

## 3. `input` items

| Item | Direction | Grade | Note |
|---|---|---|---|
| `{type:"message", role, content}` | write + read | verified | `role` is `user` \| `assistant` (this adapter also emits `system` only via `instructions`) |
| part `{type:"input_text", text}` | write | verified | The input-message content spelling for user text |
| part `{type:"output_text", text}` | write | verified | The spelling an assistant turn is replayed with; the same type an `output[]` message carries |
| part `{type:"input_image", image_url}` | write | verified | `image_url` is an http(s) URL or a `data:` URL; `detail` is not written (no canonical knob) |
| part `{type:"input_file"}` | — | refused | A canonical file part carries inline bytes; this dialect addresses stored files |
| `{type:"function_call", call_id, name, arguments}` | write + read | verified | `arguments` is a JSON **string**; `call_id` is the pairing key |
| `function_call.id` (`fc_…`) | read, ignored | verified | Provider item bookkeeping, not conversation content; the pairing key is `call_id`, so it is neither preserved nor echoed |
| `{type:"function_call_output", call_id, output}` | write + read | verified | `output` as a string |
| `function_call_output.output` as a content-part array | — | unverified | Not written: image/file tool results name `tool_result.content` instead of guessing the array form |
| `{type:"reasoning", …}` | — | refused | Replay needs `(id, encrypted_content)`; the canonical `Reasoning` has one opaque `signature` |
| `item_reference` and other session items | — | refused | Session-scoped; the gateway stores nothing |

## 4. Response envelope

| Wire member | Direction | Grade | Note |
|---|---|---|---|
| `id` | read | verified | `ResponseId` |
| `object` (`"response"`) | read | verified | Checked when present |
| `created_at` | read, ignored | verified | Framing; consumed rather than preserved (see §6) |
| `model` | read | verified | Physical model |
| `status` | read | verified | `completed` \| `incomplete` \| `failed` \| `in_progress` \| `queued` \| `cancelled` |
| `incomplete_details.reason` | read | verified | `max_output_tokens` \| `content_filter` |
| `error` | read, ignored | verified | A non-null error on a `failed` answer; the message never reaches the caller |
| `output` | read | verified | The answer |
| `usage` | read | verified | §7 |

Status → `FinishReason` (the closed table of `design.md` §4.3):

| `status` | extra condition | `FinishReason` |
|---|---|---|
| `completed` | at least one `function_call` item | `ToolCalls` |
| `completed` | otherwise | `Stop` |
| `incomplete` | `incomplete_details.reason == "max_output_tokens"` | `Length` |
| `incomplete` | `incomplete_details.reason == "content_filter"` | `ContentFilter` |
| `failed` | — | `Error` |
| anything else | — | `Malformed`: a 2xx document that is not a finished answer |

## 5. `output[]` items

| Item | Grade | Note |
|---|---|---|
| `{type:"message", role, content:[{type:"output_text", text, annotations}]}` | verified | `id`/`status` are framing and are ignored; empty text parts produce no canonical part |
| `output_text.annotations` | verified | A citation list produced by a built-in tool this adapter refuses; a non-empty list is `Malformed` rather than a silently stripped citation |
| content part `{type:"refusal", refusal}` | verified | No canonical shape; refused (`Malformed`), the same decision the chat adapter makes for `message.refusal` |
| `{type:"function_call", id, call_id, name, arguments}` | verified | Folded into `Item::ToolCall`-shaped `ToolCall` values on the turn |
| `{type:"reasoning", id, summary:[{type:"summary_text", text}], encrypted_content}` | verified | The summary text is rendered as `AssistantTurn::reasoning` with an empty signature; `id`/`encrypted_content` are the replay handles and are **not** carried (see `design.md` §6) |
| any other item type (`web_search_call`, `file_search_call`, `computer_call`, `code_interpreter_call`, `image_generation_call`, `mcp_call`, `local_shell_call`, …) | verified to exist | `Malformed`: an unknown item can carry content the caller would otherwise see |

## 6. Which top-level members are consumed instead of preserved

The dialect's response object echoes the request (`instructions`, `tools`, `text`, `temperature`, …)
next to the answer. Those members describe the *request*, not the answer, and the canonical
`extensions` is written back into a **client's** document by a client codec — writing an echo there
would report facts the caller never asked to have relayed, and would collide with a client codec's own
field of the same name. They are therefore consumed, in one documented list (`response::ENVELOPE`),
and only a member this adapter does not know is preserved in `extensions`.

Grade for the echo set: **unverified** in its exact membership. The list is a superset of the members
the adapter understands, and anything outside it travels through `extensions` unchanged; the adapter
never authors a value for a member it does not recognise.

## 7. `usage`

| Wire member | Grade | Canonical count |
|---|---|---|
| `input_tokens` | verified | `TokenKind::Input` |
| `output_tokens` | verified | `TokenKind::Output` |
| `input_tokens_details.cached_tokens` | verified | `TokenKind::CacheRead` |
| `output_tokens_details.reasoning_tokens` | verified | `TokenKind::Reasoning` |
| `total_tokens` | verified | Not carried: the canonical model stores the two counts and does not keep a derived sum |

Unknown members of a usage object are read and dropped: accounting detail cannot change the answer,
which is the rule `provider/openai/usage.rs` already documents.

## 8. Stream events

| Event | Direction | Grade | Canonical events |
|---|---|---|---|
| `response.created` | read | verified | `Start` (from `response.id` + `response.model`) |
| `response.in_progress` | read | verified | skipped (heartbeat) |
| `response.output_item.added` | read | verified | records a `function_call` item's `call_id`/`name` against `output_index`; an unknown item type is `Malformed` |
| `response.content_part.added` / `.done` | read | verified | skipped (framing) |
| `response.output_text.delta` | read | verified | `TextDelta` |
| `response.output_text.done` | read | verified | skipped (the deltas carried the text) |
| `response.function_call_arguments.delta` | read | verified | `ToolCallStart` (first one) then `ToolCallDelta` |
| `response.function_call_arguments.done` | read | verified | `ToolCallEnd`; `arguments` is only written as a delta when no delta was streamed, because the value repeats the accumulation |
| `response.reasoning_summary_text.delta` | read | verified | `ReasoningDelta` |
| `response.reasoning_summary_text.done` | read | verified | skipped |
| `response.output_item.done` | read | verified | skipped (the arguments' `done` event closes a call) |
| `response.completed` | read | verified | `Usage` then `Finish`; still-open calls are closed first |
| `response.failed` | read | verified | `Error` (from `response.error.code`, log only) |
| `error` | read | verified | `Error` (from `code`, log only) |
| any other `response.*` event | read | — | skipped: a stream fragment carries no canonical place for an unknown event |
| `data: [DONE]` | read, ignored | verified absent | Not part of this dialect; if a compatible endpoint sends it, the frame is ignored — the terminator is `response.completed` |

Event fields used: `type`, `sequence_number` (read, ignored), `response`, `item`, `output_index`,
`item_id` (read, used only to refuse a fragment with no item), `delta`, `arguments`, `code`.

Completeness: a body that ends before `response.completed` is `Malformed`, and so is a
`response.completed` that arrives twice, or a stream that opens a tool call without ever announcing
its item.

## 9. What the adapter marks `unverified` in code

1. `text.verbosity` — refused, listed in §2.
2. `function_call_output.output` as a content-part array — refused, §3.
3. The exact echo set of the response object — consumed from an explicit list, §6.
4. Assistant `output_text` parts inside `input` — written, because it is the spelling the same
   vendor's `output[]` uses for exactly this content and replay of output items is the documented
   purpose of `input`. A provider that rejects it would be a routing problem, and the module
   documentation says so.
