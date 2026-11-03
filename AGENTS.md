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

## Documentation Rules

- Design and planning documents must describe only the current final design. Do not add sections such as "corrections to the previous version", "changes from the last draft", or other comparisons against superseded designs.
- A design document must be self-contained. Readers must not need earlier conversations, drafts, or prior task context to understand it.
- Explain trade-offs as positive design principles, constraints, or alternatives. Put historical evolution, rejected options, and review history in `research/` or task records, not in the final design document.
- When revising a design, rewrite the affected section in place instead of appending a correction log.
