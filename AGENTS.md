# AGENTS.md — OpenCrabs repository directives

Directive file for AI coding agents working in this repo (and a fast index for humans).

## Read first

1. **[Ontology](src/docs/reference/ONTOLOGY.md)** — the project's shared vocabulary (SSOT: one concept = one name). Use these exact terms in code, docs, issues, and PRs. New terms go through the ontology.
2. [Brain Constitution](src/docs/reference/BRAIN_CONSTITUTION.md) — canonical reference for brain files, directives, prompt assembly (AS IS policy).
3. [CONTRIBUTING.md](CONTRIBUTING.md) — PR requirements and CI gates.
4. [TESTING.md](TESTING.md) — test conventions and the test battery.

## Project layout (map to ontology terms)

| Path | What lives here |
|---|---|
| `src/brain/` | The intelligence layer: LLM providers, agent services, tools, prompt assembly |
| `src/channels/` | Channel integrations (Telegram, Discord, Slack, WhatsApp) |
| `src/memory/` | Memory store (SQLite FTS5 + vector search) |
| `src/cron/` | Cron scheduler |
| `src/tui/` | Terminal UI |
| `src/config/`, `src/db/`, `src/utils/` | Config, database, shared utilities |
| `src/docs/` | User and developer documentation (incl. `reference/ONTOLOGY.md`) |

## Conventions

- **One concept = one name.** Terms defined in `src/docs/reference/ONTOLOGY.md` are used verbatim; do not invent synonyms. If a PR introduces or renames a concept, update the ontology in the same PR.
- **Docs are AS IS.** Documentation describes what the code does, not what it should do — desired behavior goes to issues (see Brain Constitution's standing rule).
- Rust code follows the CI gates in `CONTRIBUTING.md` (fmt, clippy `-D warnings`, all-features tests).

## Plan review

This section is injected verbatim into a plan reviewer's brief. A reviewer is a spawned sub-agent with read-only tools and no skill surface, so it cannot follow a pointer: everything it needs must be stated here.

Judge the plan on six things:

1. **The ladder.** Walk the rungs in order — does this need to exist at all → stdlib → native platform feature → already-installed dependency → one line → the minimum code that works. A step that reaches the last rung without naming why the earlier ones fail is incomplete; "we might need it later" is not a reason. Canon for the main agent: the `/ponytail` skill.
2. **A job, not a feature.** The plan must state the job in one sentence ("when X, I want Y, so I can Z"). A step that cannot be traced to that job is scope, not value.
3. **No speculative machinery.** No config knob, flag, trait or module without a requirement stated in this plan. A seam for a second caller that does not exist yet is a defect, not foresight.
4. **Grounded.** Every file, type, function and tool the plan names must exist — verified by reading it, not assumed from its name. Terms follow `src/docs/reference/ONTOLOGY.md` verbatim.
5. **Runnable acceptance criteria.** Every step names a command and the outcome it must produce. "Works correctly" is not a criterion.
6. **The gate is CI.** No step may assume a local build: fmt, clippy `-D warnings` and the test legs run in GitHub Actions (`CONTRIBUTING.md`, `TESTING.md`).

## Code review

This section is injected verbatim into a completed-plan implementation reviewer's brief. Same constraint: it must stand alone.

Judge the diff on six things:

1. **The ladder, at the diff.** Deletion over addition; the simplest change that does the job. Complexity must be paid for by a requirement named in the plan.
2. **One concept, one name.** `src/docs/reference/ONTOLOGY.md` terms used verbatim; a renamed or new concept updates the ontology in the same change.
3. **Docs are AS IS.** Documentation describes what the code does; desired behaviour goes to an issue.
4. **Tests exercise what they name.** A green leg that cannot redden proves nothing — a `hasnt` leg forbidding a literal the same change rewrote, or a leg that passes because an earlier gate refused, is decorative. Assert the drop, not an absolute count, and never relax an assertion to make a guard pass.
5. **The gate is CI.** fmt, clippy `-D warnings`, `cargo nextest` and `cargo test --doc` in GitHub Actions — never a local build, and never "tests pass" without a run receipt.
6. **Receipts, not recollection.** Every claim in the review cites the command or run that produced it; a review that reports a state it did not read is a fabrication, not a finding.

