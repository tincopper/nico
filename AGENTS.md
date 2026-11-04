<!-- TRELLIS:START -->
# Trellis Instructions

These instructions are for AI assistants working in this project.

This project is managed by Trellis. The working knowledge you need lives under `.trellis/`:

- `.trellis/workflow.md` — development phases, when to create tasks, skill routing
- `.trellis/spec/` — package- and layer-scoped coding guidelines (read before writing code in a given layer)
- `.trellis/workspace/` — per-developer journals and session traces
- `.trellis/tasks/` — active and archived tasks (PRDs, research, jsonl context)

If a Trellis command is available on your platform (e.g. `/trellis:finish-work`, `/trellis:continue`), prefer it over manual steps. Not every platform exposes every command.

If you're using Codex or another agent-capable tool, additional project-scoped helpers may live in:
- `.agents/skills/` — reusable Trellis skills
- `.codex/agents/` — optional custom subagents

Managed by Trellis. Edits outside this block are preserved; edits inside may be overwritten by a future `trellis update`.

<!-- TRELLIS:END -->

## Task Rules

- **Do not create a Trellis task for record-only or mechanical work**: task-directory or slug
  renames, journal/index edits, typo and wording fixes, dependency bumps that change no code.
  Do it inline, verify it, and leave the commit for the user to request.
- Create a task when the change touches code, a binding spec contract, or observable behaviour:
  those need a PRD (plus `design.md` / `implement.md` when complex) before implementation starts.
- When the class of a change is genuinely ambiguous, ask once — not once per instance.

## Documentation Rules

- Design and planning documents must describe only the current final design. Do not add sections such as "corrections to the previous version", "changes from the last draft", or other comparisons against superseded designs.
- A design document must be self-contained. Readers must not need earlier conversations, drafts, or prior task context to understand it.
- Explain trade-offs as positive design principles, constraints, or alternatives. Put historical evolution, rejected options, and review history in `research/` or task records, not in the final design document.
- When revising a design, rewrite the affected section in place instead of appending a correction log.

## Code Rules

- Setup, hooks and daily commands: `CONTRIBUTING.md`.
- Follow `.trellis/spec/backend/` for all Rust work; it is the binding contract, not a suggestion.
- Default to first-principles design: state the invariants and failure modes first, then choose the
  structure. Every abstraction must name the coupling it removes and its second real consumer.
- High cohesion, low coupling, extensibility at existing seams: one business boundary (or one data
  structure) per module; dependencies point at the domain owner; no new registry/factory until a
  second implementation exists.
- Local gates are orchestrated by lefthook (`lefthook.yml`, `lefthook install`); a red pre-commit or
  pre-push hook must be fixed, not bypassed (`SKIP_NICO_HOOKS=1` is for tool outages only and
  must be mentioned in the commit). Never set `core.hooksPath` while lefthook manages hooks.
- File budget: 300–500 lines per `.rs` file, **never above 500**. Split by boundary into
  `foo.rs` + `foo/` submodules instead of growing a file. Existing oversized files are refactor
  backlog — split before adding lines.

## Commit Rules

- Never run `git commit` (or any command that commits, including `task.py archive` without `--no-commit`) unless the user explicitly asks for a commit in that turn.
- When work is finished, leave the changes in the working tree, report what changed and the proposed commit message, then wait.
- `task.py archive` auto-commits by default; always pass `--no-commit` and commit only on request.
- Sub-agents are bound by the same rule: their dispatch prompt must state that they may not commit.
