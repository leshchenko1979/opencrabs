# OpenCrabs Ontology — Shared Vocabulary (SSOT)

> **Owns:** the project's shared/core vocabulary — one concept = one name, defined once, used everywhere.
> **Status:** AS IS (describes terms as the codebase uses them today)
> **Audience:** every contributor — human or LLM agent — writing code, docs, issues, or PRs in this repo.

**Law of this file:** a concept gets ONE name. If you need to name a new concept, propose it here first. If an existing term fits, use it — never invent a synonym. Domain slices (brain, plan-mode) stay canonical inside their own docs; this file defines the shared core and links out. Definitions describe what the code DOES (AS IS) — behavior changes go through issues, not redefinitions.

**Why an ontology matters here:** the primary readers of this repo's docs and prompts are LLM agents. Unanchored vocabulary produces invented synonyms, misrouted tool calls, and conflated concepts (the codebase already carries `tool_name_heal.rs` and `phantom.rs` to absorb exactly this failure class). One grounded name per concept is cheaper than healing.

## How to read this file

| Mark | Meaning |
|---|---|
| *(term)* | the canonical name — use exactly this |
| **Not:** X, Y | known synonyms/wrong names — do not use them |
| → | deeper definition lives in the linked slice; that slice is canonical for that domain |

**A `**Not:**` entry bans a CONCEPT NAME, not a character string.** Code identifiers, module and file names, and CLI strings legitimately carry these words and are NOT vocabulary violations — `plan_files.rs` and the `🦀 Memory Files` listing are the names the code ships, and this file describes the code AS IS. A `**Not:**` entry tells you what to call the *concept* when you write prose, docs, issues, or prompts; it never says an identifier must be renamed.

## Core concepts

### Runtime & identity

**OpenCrabs** — the binary and project: an AI orchestration agent with tools, channels, memory, and skills.

**Profile** — an isolated instance configuration rooted at `~/.opencrabs/profiles/<name>/` (own config, keys, brain files, sessions). The default install runs at `~/.opencrabs/` with no profile subdirectory. **Not:** "instance", "workspace".

**Session** — one conversation with its own context window, history, and tool loop, identified by a UUID (`session_id`). Sessions belong to a profile. **Not:** "chat" (a chat is a channel-side conversation; one chat can drive a session, sessions can exist without a chat).

**Channel** — a messaging surface connected to OpenCrabs (Telegram, Discord, Slack, WhatsApp, Trello). The harness routes inbound channel messages into sessions and outbound replies back. **Not:** "surface", "connection".

**A2A** — Agent-to-Agent protocol: JSON-RPC 2.0 peer communication between OpenCrabs and remote agents (agent card discovery, task send/get/cancel). See `src/a2a/`.

**Cron job** — a scheduled task in the `cron_jobs` table, polled by the scheduler and executed in the user's active session. Defined by prompt + schedule; delivery goes through a configured channel. See `src/cron/`. **Not:** "timer", "task" (a task is a plan checklist row).

### Channels & delivery

**Delivery mode** — how a `session_notify` reaches its target. `turn-end` (the default) queues the message for the target's next tool-loop boundary: against an IDLE target that is immediate delivery, against a BUSY one it queues instead of being dropped. `quiet` defers until the target has been idle for `quiet_for_secs` (any turn activity restarts the clock; `max_delay_secs` forces delivery into a busy turn). The retired `now` mode refused a mid-turn target instead of queueing and is now rejected (#373). **Not:** "interrupt" — that legacy argument is accepted but selects no behaviour.

**Local image attachment** — an image on the local filesystem that a reply references and OpenCrabs delivers as native channel media, instead of leaving the reference in the message body as dead text. Two reference forms resolve to it: the **marker form** `<<IMG:/abs/path.png>>` and the **markdown form** `![alt](path)` (absolute, `~/…`, or relative to the session working directory). A remote `http(s)://` or `data:` target is fetched and delivered by the same path but is not a local file. Extraction is single-homed in `src/utils/image.rs` (`extract_local_images`; `strip_image_references` is the strip-only variant for surfaces that never send media; remote targets are resolved by `src/utils/image_fetch.rs`). **Not:** "image marker" as a name for the markdown form (a marker is the `<<IMG:…>>` form only), "attachment" alone (that names the channel-side artifact, not the reference).

### Brain & directives

**Directive** — any information that shapes agent behavior: rules, tool definitions, skills, commands, config, project knowledge. → brain slice (`BRAIN_CONSTITUTION.md`).

**Brain file** — a markdown file in the profile home that shapes agent behavior; each declares what it owns in an `Owns:` header. Core files are always in the system prompt; contextual files load on demand via `load_brain_file`. → `BRAIN_CONSTITUTION.md` §1 (canonical for all brain-file subtypes). **Not:** "memory file" as a name for a brain file — memory is one brain file among several, and the `memory/` daily logs are a separate thing.

**Skill** — a reusable multi-step workflow defined in `SKILL.md` with YAML frontmatter; user skills live in `<profile home>/skills/<name>/`, built-in skills are embedded in the binary. **Not:** "command" (a command is a slash mapping, below).

**Command** — a user-defined `/<name>` slash command in `commands.toml`, mapping to a prompt or system action. **Not:** "skill".

**Dynamic tool** — a runtime tool definition in `tools.toml` adding a callable tool without recompiling. Distinct from an extended tool, which is compiled into the binary and lazily surfaced via `tool_search`.

### Tools & the loop

**Tool** — a callable function the model invokes through the structured tool-call API. **Not:** "function", "action".

**Core tool** — schema sent to the LLM on every request. **Extended tool** — schema surfaced only after `tool_search` activates it. → `BRAIN_CONSTITUTION.md` §1.

**Tool call** — one structured invocation of a tool (emitted between turns; results arrive as tool results). A tool call that is *described* in prose but never emitted is a phantom tool call — the codebase's phantom-heal machinery exists because of it. **Not:** "command run".

**Tool result** — the output returned to the model after a tool call. The only evidence a tool executed.

### Memory

**Memory search** — retrieval over past daily logs (`memory` scope), brain files (`brain` scope), and indexed external paths (`external` scope). See `src/brain/tools/memory_search.rs`. **Not:** "recall" as a synonym for the tool (recall is the ranking subsystem inside it).

**Daily log** — the per-day log at `memory/YYYY-MM-DD.md` that records what happened in past sessions. Distinct from brain files (rules/policy).

### Plan mode

**Plan** — a session's structured plan: design prose plus an executable checklist, stored as JSON sidecar (`.opencrabs_plan_<session-id>.json`) mirrored to a session `.md`. Managed by the `plan` tool. **Not:** "task list" alone (a plan holds the design and its execution track), "roadmap".

**Session plan / checklist** — the two representations of a plan: the design `.md` (Editing status, user-reviewable) and the executable `tasks[]` (Active status). **Not:** "todo list". ("Plan file" is the on-disk artifact's own name — the module is `src/utils/plan_files.rs` — not a name for the concept.)

**Design track / checklist track** — how a plan starts: design goes to Editing and waits for user Approve; a checklist with inline tasks goes Active immediately. → plan-mode slice (`plans/plan-mode/README.md`).

**ADR** — Architecture Decision Record: a numbered doc capturing one decision cluster with phases and done criteria (used by the plan-mode slices). → `plans/plan-mode/README.md`.

### Goals & the judge

**Goal** — a session's declared objective that the agent works toward across turns, stored in the `goal_state` table keyed by `session_id` and bounded by a turn budget. Set by the `/goal` command or the `goal_manage` tool. **Not:** "task" (a task is a plan checklist row), "intent", "mission".

**Criterion** — one declared, checkable condition a goal is judged against; a goal carries a list of them, identified `c1`, `c2`, … . Supplied at `set` time or derived once from the goal text. **Not:** "acceptance criteria" (that is the plan-task field).

**Criterion status** — how one criterion fared against the evidence: `MET`, `UNMET`, or `NO_EVIDENCE`. `NO_EVIDENCE` is what any absent, blank, or garbled status degrades to — the one status that cannot verify a goal.

**Goal verdict** — the aggregate outcome of a judge call, computed deterministically in Rust from the per-criterion statuses: `VERIFIED` (every criterion `MET`), `REJECTED` (any `UNMET`), `UNCERTAIN` (otherwise, including a goal that declares no criteria). Only `VERIFIED` ends a goal. **Not:** `DONE` / `CONTINUE` as verdict names — those name loop decisions (`GoalDecision`), not verdicts; a legacy `DONE` row reads as `VERIFIED`.

**Evidence pack** — the mechanical facts the runtime supplies to the judge: running background tasks, unresolved plan tasks, and the turn budget used/max. **Not:** "context" (the judge is given no session history, tool results, or plan text).

**Mechanical gate** — the pre-judge short-circuit: while a background task is still running or a plan task is unresolved, the goal loop acts **without** consulting the model. The two cases then diverge (#567): an unresolved **plan task** is work the agent can advance, so the loop re-prompts and bills the turn; a **running background task** is work it is *waiting on*, so the loop defers — the turn ends, the budget is untouched, and an await record arms the completion wake. Both are machine-readable signs the agent's own prose cannot make untrue.

**Uncertain streak** — consecutive `UNCERTAIN` verdicts on a goal. At `MAX_CONSECUTIVE_UNCERTAIN` (3) the goal parks as `paused`, naming the exhausted evidence budget; a `VERIFIED` or `REJECTED` verdict resets the streak.

### Session recovery

**Await record** — the three `session_bindings` columns (`await_kind`, `await_ref`, `await_at`) recording that a session ended its turn waiting on an EXTERNAL completion: a CI run, a peer lane, an owner reply. `await_at IS NULL` means not awaiting, so pre-feature rows keep their classification. Read by an OR-path beside the boot classifier's freshness gate (`WAKE_RECENT_SECS`), so a stale binding carrying a wait is still classified. **Not:** "parked" (that is the route sense — a report waiting for a channel route, `src/brain/agent/service/restart_recovery.rs`), "blocked", "pending".

**Awaiting** — the boot-classifier outcome for a session carrying an await record. It is resumed through the same `resume_session` continuation an `interrupted` turn uses — no second wake mechanism. Distinct from the freshness gate, which is unchanged: this adds an eighth signal, it does not widen the window. **Not:** `interrupted` (a turn killed in flight), `unclassified` (no evidence either way).

### Fleet & process terms

These live with the ops skill outside this repo (`fleet-directives.md` §Glossary: carrier, fan-out, lane, roster, CI gate, ORDER gates, single-flight, GREEN/RED, S2/S3). They are listed here by NAME ONLY so repo readers know the terms exist and where they are defined — this file does not copy them (single-writer law).

## Maintenance

- **Adding a term:** PR-only. Define AS IS, add NOT-references for any synonym you found in the wild, link the canonical slice if one exists.
- **Changing a definition:** the definition must match what the code does. If code and ontology disagree, file an issue — do not redefine the term to match intent.
- **Drift check:** when a domain slice renames or retires a term, the same PR updates the references here.
- Do not duplicate a domain slice's terms here — link them (one concept, one home).
