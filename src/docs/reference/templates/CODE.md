# CODE.md — How You Write Code

> **Owns:** how you write code — standards, file organization, testing, security-first, and the Rust-First Policy.

*You build things. Build them right.*

---

## Philosophy

**Single binary. Run it. Delete the mess. Move on.**

You are not a framework junkie. You don't leave build artifacts rotting on disk. You compile, verify it works, clean up, and ship. If something needs to change — rebuild from scratch. Binaries are disposable. Source is sacred.

**Rust first. Always.** When choosing a language, Rust wins unless there's a concrete reason it can't (browser JS, platform SDK requirement, etc.). Native, safe, fast, single binary. No runtime dependencies. No "just install Node and Python and Java and..." — one binary, done.

**Small is beautiful.** Every line of code is a liability. Less code = less bugs = less maintenance. If you can solve it in 50 lines, don't write 200. If a dependency does it well, don't rewrite it. But if a dependency drags in half the internet, write it yourself.

---

## File Organization

### Hard Limits

- **No file over 500 lines.** If you're approaching that, you've already waited too long to split.
- **No file over 300 lines** without a damn good reason and a plan to refactor.
- **Target: 100-250 lines per file.** That's the sweet spot. Easy to read, easy to review, easy to test.

### Structure

Every module follows this pattern — **separate concerns into separate files:**

```
feature/
├── mod.rs          # Public API, re-exports (thin — just wiring, zero logic)
├── types.rs        # Structs, enums, type aliases
├── handler.rs      # Request handling / business logic
├── utils.rs        # Helper functions (only if needed)
└── tests/
```

### Module Declarations (`mod.rs`)

**`mod.rs` files are strictly for wiring and re-exports.**

- `mod.rs` MUST contain only: module doc comments, `pub mod` declarations, and `pub use` re-exports.
- **Zero function definitions (`fn`) inside `mod.rs`.** Logic in `mod.rs` creates circular dependencies, bloats the module root, and obscures the public API.
- If a helper belongs to the module, put it in `utils.rs`, `service.rs`, or a dedicated submodule, then `pub(crate) use` or `pub use` it.

---

## Coding Standards

### Scan Before You Write

- Before writing ANY formatting, rendering, or parsing helper in an existing codebase, sweep for reusable primitives first (`grep` / code search).
- Reuse granularity = data shape: one-line items get the renderer's inline primitives, not the document pipeline.
- Private copies of exported helpers found during a sweep are DRY targets — delegate to the canonical helper in the same change rather than proliferating duplicates.

### Rust-First Policy

This is the single home for the Rust-first rule (AGENTS.md and BOOT.md point here).

When searching for new integrations, libraries, or adding new features, **always prioritize Rust-based crates** over wrappers, FFI bindings, or other-language alternatives. Performance is non-negotiable — native Rust keeps the stack lean, safe, and fast. Only fall back to non-Rust solutions when no viable crate exists.

### Rust Cross-Module Visibility & Compilation Error Masking (E0433 Class)

1. **Struct-field additions must cover ALL initializer shapes:** Textual scans for `TypeName {` literals miss `Self { … }` constructors inside the type's own `impl` block. Search for both `TypeName {` and `Self {` in the defining module when adding or altering struct fields.
2. **Error-stage masking (E0433 aborts before type checking):** A name-resolution or module visibility error (E0433) halts the compiler before full type-checking and borrow-checking run. Fixing an E0433 will often expose a second wave of downstream errors (E0063, E0277, E0308). Budget for multi-round resolution and never assume the initial error list is exhaustive.
3. **Teloxide request builders are move-chained:** `Recipient` has no `From<i64>` (wrap in `ChatId(x)`), `message_thread_id` takes bare `ThreadId`, and optional parameters require chained mutation:
   ```rust
   let mut req = bot.send_message(chat_id, text);
   if let Some(thread_id) = thread_id {
       req = req.message_thread_id(thread_id);
   }
   ```
   Never pass an `Option<T>` directly into a builder setter expecting `T`.

### Manifest Block Edits

After replacing or rewriting a multi-line dependency block in `Cargo.toml` (or any package manifest), re-read the entire surrounding section and check declaration↔reference consistency before committing:
- Partial edits can silently drop required sibling features or dependencies while leaving feature flags referencing them.
- Always check that every `dep:crate` entry in the `[features]` table has a matching entry in `[dependencies]`.

### Glue-Boundary Contract Verification

Before integrating two third-party crates or architectural layers, verify the producer's actual output contract from its source fixtures and integration tests — never from the function signature alone:
- Function signatures (e.g. `fn render() -> Result<String>`) indicate return types, not semantic data contracts (e.g. returning an inner SVG XML fragment instead of a complete root `<svg xmlns="...">` document).
- Inspect producer test fixtures and snapshot outputs to verify the exact data shape before passing it to strict consumers.
- Signature-compatible does NOT mean contract-compatible; verify integration across boundaries with dynamic test execution.

### Journal & Telemetry Logging — Never Log Raw Text Tails

Structured log writers must sanitize embedded newlines:
- Any log field containing raw user input or unescaped text (e.g. `text=`) will split the log record at the first `\n`, detaching subsequent key-value pairs onto orphan lines and corrupting automated log parsers.
- Always sanitize variable text fields: `.replace('\n', "\\n").replace('\r', "\\r")`.
- Place free-form text fields at the end of structured log formats so that unexpected delimiters cannot break preceding index counters or timestamps.

### Teloxide Payload Setters Are Traits

Payload setter methods (such as `EditMessageReplyMarkupSetters`) live in traits re-exported via `teloxide::prelude::*`.
- When writing a module that imports Teloxide items explicitly (without `use teloxide::prelude::*;`), calling a payload setter will fail with a deceptive `E0599` error ("`reply_markup` is a field, not a method").
- When modifying message payloads in explicit-import files, explicitly import the corresponding `...Setters` traits.

### Call-Site ↔ Definition Shape: fmt-clean is NOT Compile-Clean

Code formatting tools (`cargo fmt` / `rustfmt`) verify layout and syntax style only. Full correctness, borrow checking, and test suites are verified via GitHub Actions CI workflows (`.github/workflows/` — `cargo fmt --check`, `cargo clippy`, `cargo test --all-features`). A file that is 100% format-clean can still harbor fatal compilation and borrow errors:
1. **Associated vs. Free Functions (E0425):** Calling `module::helper_fn()` when the function is defined as `module::Type::helper_fn()`.
2. **Moving out of `&mut self` in `Drop` implementations (E0507):** Calling `drop(self.field)` inside a `Drop::drop` implementation moves a field out of a mutable reference. Correct pattern: wrap the field in `Option<T>` and use `self.field.take()`.
3. **Rule:** Mechanically cross-check every new or altered call site against the callee's exact definition shape (associated vs free function, receiver mutability `&self` vs `&mut self`, and `Drop` move semantics). Never treat a clean format check as proof of valid code — code verification gates run strictly in GitHub Actions CI workflows.

---

## Testing Standards

1. **Test Isolation (NO Inline Tests in `src/`):** All unit and integration tests must reside in `src/tests/*_test.rs` and be registered in `src/tests/mod.rs`. Absolutely **NO inline `#[cfg(test)] mod tests { ... }`** blocks inside production files under `src/`. Inline tests clutter source files and obscure module structure.
2. **Commit Trailers (Zero `Co-Authored-By`):** Never include `Co-Authored-By` trailers in commit messages.
3. **Real Tests Over Mocks:** Write tests that exercise real structs, file systems, and embedded databases (SQLite) rather than artificial, fragile mocks.
4. **Zero Error / Warning Suppression:** Never commit `#[allow(dead_code)]`, `#[allow(unused)]`, or warning suppression attributes. Dead or unused code must be deleted immediately.
5. **`ONTOLOGY.md` Synchronization:** Shared domain vocabulary lives in `src/docs/reference/ONTOLOGY.md`. If a change introduces, renames, or deprecates an architectural concept, update `ONTOLOGY.md` in the same PR.
6. **No Clock-Bomb Fixtures in Tests:** Never write test fixtures with hardcoded absolute future timestamps or recency horizons. Tests must use simulated clocks, relative time offsets (`Utc::now() - Duration::hours(1)`), or date-independent invariant checks.
7. **Dynamic Invariants Over Frozen World-State Fixtures:** Derive expected properties dynamically from the input dataset under test (e.g. `leader = routes.iter().min_by_key(...)`) and assert the invariant rule rather than a frozen external literal.
8. **Cross-Boundary Unit Test Requirement:** Tests asserting paths, serialization formats, or contracts must verify real cross-boundary I/O (e.g. writing through a real file writer and reading back via the target parser in a temporary directory) rather than tautologically asserting equality against an internal helper delegate.
9. **CI Gate Verification:** All code changes and pull requests must pass the GitHub Actions CI workflow matrix (`.github/workflows/` — fmt, clippy, and full multi-feature test suite) before deployment or merge.

---

## LLM Ergonomics & Efficiency Standards

Every tool schema, hint, error message, and prompt interface designed for AI orchestration must prioritize **LLM ergonomics and operational efficiency**:

1. **Contextual & Relative Anchors Over Mental Arithmetic:** Never require an agent to calculate or predict absolute numeric array indices, character counts, or line offsets across turns. Prefer semantic anchors (fuzzy substring matching, title matching) or relative position tokens (`"current"`, `"next"`, `"tail"`).
2. **Actionable, Self-Healing Feedback:** Error messages are prompts. Never emit bare negative rejections (`"Invalid index"`, `"Task not found"`). Validation errors must provide an immediate snapshot of current state along with the valid candidate options so the agent can self-heal in a single turn without exploratory round-trips.
3. **Turn Consolidation & Immediate State Delivery:** Mutating tool operations should return both confirmation of the mutation and the resulting state summary (e.g. `add_tasks` returning the updated plan). Eliminate empty mutations that force immediate read follow-up turns.
4. **Information Saliency & Token Discipline:** Prompts, descriptions, and error payloads must maximize signal-to-noise. Eliminate verbose conversational filler, state operational constraints explicitly, and maintain compact schemas to minimize context window bloat and compaction pressure.
