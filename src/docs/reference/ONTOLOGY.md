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

**Retained document** — a `.md` file a session read WHOLE, outside the three classes that are not documents: a skill body (own class, above), a brain file (the prompt builder's job), and a file the session WROTE (a plan, a report, an evidence file). Tracked per session at the read site (`read_state::mark_document_read`, canonicalised, capped) and carried across compaction in three steps: listed as a `Document` row in the context inventory the summariser reads, named back by the summariser in the manifest's `retained_documents` section, and — only if the session really did read that path in this process — re-injected by absolute path as `## Retained documents` from the retained-set leftover, floored at 10% of the context window. The point is project law that lives in a repo (`CLAUDE.md`, `AGENTS.md`, a project's own `docs/*.md`): it is read, obeyed, and would otherwise be forgotten at the next compaction, because the retention machinery knew only `skills/<slug>/`. **Not:** "skill" (a skill is registered by slug and re-injected whole; a document is registered by path and shares the leftover budget), "context manifest" (the manifest is the summariser's ANSWER — the section that names which skills and documents to carry — while a retained document is one of the things it names), "brain file" (brain files are rebuilt from disk by the prompt builder on every turn, so they never needed retaining).

**Dynamic tool** — a runtime tool definition in `tools.toml` adding a callable tool without recompiling. Distinct from an extended tool, which is compiled into the binary and lazily surfaced via `tool_search`.

### Tools & the loop

**Tool** — a callable function the model invokes through the structured tool-call API. **Not:** "function", "action".

**Core tool** — schema sent to the LLM on every request. **Extended tool** — schema surfaced only after `tool_search` activates it. → `BRAIN_CONSTITUTION.md` §1.

**Tool call** — one structured invocation of a tool (emitted between turns; results arrive as tool results). A tool call that is *described* in prose but never emitted is a phantom tool call — the codebase's phantom-heal machinery exists because of it. **Not:** "command run".

**Tool result** — the output returned to the model after a tool call. The only evidence a tool executed.

**Serialised read-modify-write** — a mutation that holds a per-target lock across the WHOLE read-then-write, not the write alone. The lock is taken BEFORE the read: taken after, two writers share one baseline, the later save replaces the target outright, and both callers still receive success — so the loss is silent (a *lost update*). Two mechanisms, one per store kind: `src/brain/tools/path_lock.rs` for path-addressed files (`edit`, `write`, `hashline`, `session_context`), and the `state` slot in `src/utils/plan_files.rs` for the plan sidecar (`mutate_plan`, "the single serialised read-modify-write for a session's plan", #506). Both are ADVISORY — a contended writer waits briefly, then proceeds and REPORTS the overlap rather than failing, so the notice is part of the contract, not decoration. **Not:** "atomic write" (that names the temp-file-plus-rename publish that `ContextStore::save` and `plan_files::save_plan` use — it prevents TORN writes and says nothing about lost updates), "file lock" alone (it names the mechanism, not the scope of the hold).

### Context & compaction

**Compaction marker** — a `user` row in `messages` whose content BEGINS with `[CONTEXT COMPACTION` — the durable record that a compaction happened. Anchored at the row start, never `contains`, so an assistant row quoting the banner (or a lane reading it out of a log) is not a marker (#175). Classified in `src/brain/agent/compaction_boundary.rs`. **Not:** "compaction row" as a synonym (a row is a marker only if it classifies as one).

**Marker kind** (`MarkerKind`) — the three shapes a marker row can take: `Restart` (history before it was summarised away), `DeltaSegment` (a frozen segment appended to the window its restart opened), `Suspended` (the loop guard / non-reducing brake declined to compact, so nothing was summarised). One classifier answers for every reader — the SQL loader, the in-memory trim and the LLM-row cleaner — because two implementations of one rule drift (#987).

**Compaction boundary** — the row a reload window starts at: the NEWEST `Restart` marker, in `rowid` (insertion) order — never `sequence`, which is not authoritative because a delete once let a later insert reuse an earlier number (#730). A `DeltaSegment` or `Suspended` row is a marker but NOT a boundary: a segment extends the window its restart opened, and a suspended row summarised nothing, so anchoring there would drop live history. **Not:** "last marker" (the newest marker and the boundary are different rows).

**Compaction window** (`CompactionWindow`) — the reload payload of `MessageRepository::load_compaction_window`: `messages` (boundary through tip), `total_rows`, and `boundary: Option<BoundaryMeta>` naming the boundary row's id, rowid and kind. It carries its own provenance so the skipped-row count is reported once, at load, instead of being re-derived by each reader (#987).

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

These live with the ops skill outside this repo (`SKILL.md` §Glossary: carrier, fan-out, lane, roster, CI gate, ORDER gates, single-flight, GREEN / RED, S2 / S3). They are listed here by NAME ONLY so repo readers know the terms exist and where they are defined — this file does not copy them (single-writer law).

## Maintenance

- **Adding a term:** PR-only. Define AS IS, add NOT-references for any synonym you found in the wild, link the canonical slice if one exists.
- **Changing a definition:** the definition must match what the code does. If code and ontology disagree, file an issue — do not redefine the term to match intent.
- **Drift check:** when a domain slice renames or retires a term, the same PR updates the references here.
- Do not duplicate a domain slice's terms here — link them (one concept, one home).
