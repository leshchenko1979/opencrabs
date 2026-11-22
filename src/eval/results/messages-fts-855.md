# #855 — a trigram FTS index over `messages.content`

**Date:** 2026-11-22
**Issue:** [leshchenko1979/opencrabs#855](https://github.com/leshchenko1979/opencrabs/issues/855)
**Branch:** `fix/855-messages-fts`
**Verdict:** the index is built, gated and wired; **every number below is from a
synthetic scratch corpus, not the live store.** The live timings are unmeasured
until this ships — see [§6](#6-what-is-not-measured).

## 1. The defect

`session_search`'s content arms are unscoped `content LIKE '%x%' COLLATE NOCASE`
scans. On the ops store:

| Quantity | Measured |
|---|---|
| `messages` rows | 43,756 |
| `sum(length(content))` | 607,204,493 bytes |
| mean row | 13,872 chars |
| indexes on `messages` | `idx_messages_created_at`, `idx_messages_session_id`, PK autoindex — **none on `content`** |
| time per content query | **~75 s** (#855) |

## 2. Design

**Tokenizer: `trigram`, not `unicode61`.** The shipped ranker and the #941 eval
measure SUBSTRING semantics — `brown` matches `rown`, mid-word included.
`unicode61` matches whole tokens at word boundaries, which would silently change
the result set and invalidate the #941 measurements. Verified on this box
(bundled SQLite 3.53.2): `MATCH '"rown"'` finds `brown`.

**The index is a PREFILTER; `LIKE` stays the authority.** Both SQL arms gain

```sql
AND rowid IN (SELECT rowid FROM messages_fts WHERE messages_fts MATCH ?)
```

and keep their `content LIKE ? ESCAPE '\' COLLATE NOCASE` term. The prefilter can
only ever be a **superset** of the LIKE arm — trigram case folding is
Unicode-aware while `LIKE … NOCASE` folds ASCII only, and a quoted FTS5 phrase is
a contiguous character match — so splicing it changes the query PLAN and never
the result set. That property is pinned by test 5 below on a hostile fixture.

**One clause, four call sites.** `PREFILTER_CLAUSE` is a single constant in
`crate::db::messages_fts`, spliced into `contiguous_rows`, `tokenized_rows` and
the Track A harness's `search_slice`. A harness that built its own clause text
would measure a query the binary does not run (#911).

**Terms shorter than one trigram.** A 1- or 2-character term cannot be a trigram
and `MATCH` answers **zero rows** for it rather than erroring — the silent,
dangerous shape. Any such term makes `FtsPrefilter` yield `None` and the whole
query falls back to the LIKE-only scan. The term is never silently dropped.

**Structure only, in the migration.** The migration creates the empty index, its
state row and three sync triggers. The 607 MB copy is a detached, chunked,
resumable backfill (`CHUNK_ROWS = 200`, 25 ms pause between chunks, ≤ 3 passes),
started off the startup path. `messages_fts_state.complete` gates the prefilter,
so until the backfill finishes every search runs the pre-#855 scan unchanged.

**Invalidated on VACUUM.** `messages` has a TEXT primary key, so SQLite may
renumber its rowids during a `VACUUM` — and `messages.rowid` is the index's key.
The rowids survived on this box, but a silent drop is the failure class this
issue exists to remove, so any VACUUM that ran marks the index stale and the next
boot's backfill rebuilds it.

## 3. Deviation from the parent's stated design — `content=''`, not `content='messages'`

The parent specified an external-content index (`content='messages'`,
`content_rowid='rowid'`). **That shape is broken for this table**, for three
measured reasons, each reproduced in a scratch store:

1. **It breaks every `DELETE FROM messages` until the index is complete.** On an
   external-content table, both the `'delete'` command form and a plain
   `DELETE FROM fts WHERE rowid=?` raise `SQLITE_CORRUPT` (code 11) when that
   rowid is not in the index. The migration creates an EMPTY index over 43,756
   existing rows, so between the migration and the end of the backfill every
   session delete, retention prune and `delete_by_session` would fail **on a user
   request**. Reproduced: one `DELETE FROM messages` against a freshly migrated
   store → `Runtime error near line 5: database disk image is malformed (11)`.
2. **Its completeness check is vacuous.** `SELECT count(*) FROM messages_fts` on
   an external-content table is answered by the CONTENT table, so it always
   equals `count(*) FROM messages`. The backfill could never tell whether it was
   needed and no test could prove the index was in sync.
3. **It cannot be backfilled in chunks.** `rebuild` is all-or-nothing, so it
   holds the write lock for the whole 607 MB pass and blocks every user write.

Contentless + `contentless_delete=1` (SQLite ≥ 3.43; bundled is 3.53.2) has none
of those properties: delete of an absent rowid is a harmless no-op, `count(*)`
reports the real indexed row count, and rows are added incrementally. Nothing
ever reads `messages_fts.content` — the index is a prefilter and the LIKE arms
decide membership — so not storing the text costs nothing.

**Verified in a scratch store** (50 rows inserted *before* the index existed,
matching the live store's shape): migration leaves `indexed = 0` and
`complete = 0`; a `DELETE` of an unindexed rowid succeeds; the chunked backfill
brings `indexed` to 49 = `messages`; a second `INSERT OR REPLACE` of every row
leaves the count at 49 (no stale duplicates); `invalidate` returns it to
`indexed = 0, complete = 0`.

## 4. Measured performance (synthetic corpus — over-estimates the live store)

Corpus: 2,000 rows / **27,743,983** content bytes, synthetic Zipf-distributed
text. Generated and measured in `/tmp/ftsbench/`.

| Metric | Value | Scaling to the live 607.2 MB |
|---|---|---|
| Index size | 49,217,536 B for 27.7 MB content = **1.77×** | ≈ **1.07 GB** |
| Bulk build rate | **4.15 MB/s** (27.7 MB in 6.7 s) | ≈ **2.4 min** |
| Chunked build rate (200-row `INSERT OR REPLACE`) | **3.62 MB/s** | ≈ **2.8 min** |
| Write overhead per row | 0.15 ms → 2.18 ms with triggers (**+2.03 ms**) | — |

Query timings on that corpus:

| Query shape | LIKE only | With prefilter | Ratio |
|---|---|---|---|
| Mid-selectivity term (df 85) | 92.3 ms | **8.41 ms** | **11.0×** |
| Common term (df 2000 / 2000) | 174 ms | 179 ms | 1.0× (no regression) |
| Two-token AND | 136.6 ms | 161.6 ms | 0.85× (see note) |

The two-token case is *slower* with the prefilter on this corpus. That is
expected and it is why the prefilter is a superset rather than a replacement: the
FTS probe is paid up front, and when a term is in nearly every row the probe buys
nothing while the LIKE scan still runs over the same rows. It is reported rather
than hidden — the win is on selective terms, which is what a real query is.

**Disk headroom is tight and must be checked before ship:** `/` had 8.5 G free
against a 1.25 GB database plus a ~1.07 GB index.

## 5. Result-set parity

`rown`, `brown`, `BROWN`, `quick brown`, `café`, `CAFÉ`, `100%`, `50%`,
`under_score`, `more_underscores`, `a-b`, `ab`, `quote "` — each run against the
same fixture with `complete = 1` and `complete = 0`, over all four SQL shapes
(both arms × with and without a session filter): **zero mismatches.** The same
check ran as SQL against a scratch store before the Rust existed, and as test 5
in `src/tests/messages_fts_test.rs`.

## 6. What is NOT measured

**Live-store timings are unmeasured until ship.** Every figure above comes from a
synthetic 27.7 MB corpus; the live store is 607.2 MB with a different length
distribution, so both the 2.4-minute build and the 11× query win are projections.

The parent's confirming command, after this ships and one backfill completes:

```sh
sqlite3 "file:$HOME/.opencrabs/profiles/ops/opencrabs.db?mode=ro" \
  "SELECT (SELECT count(*) FROM messages) AS rows,
          (SELECT count(*) FROM messages_fts) AS indexed,
          (SELECT complete FROM messages_fts_state WHERE id=1) AS complete;"
# rows == indexed and complete == 1  → the index is live

# then the before/after on the same query:
sqlite3 "file:$HOME/.opencrabs/profiles/ops/opencrabs.db?mode=ro" \
  "SELECT count(*) FROM messages WHERE content LIKE '%flaky%' COLLATE NOCASE;"
```

Read the store in place with `mode=ro`; **never copy the live database.**

## 7. Out of scope, deliberately

- `session_search`'s **default scope stays `all`**. Narrowing it would change
  what users get, which is a product decision, not a perf fix. Deferred.
- The ranker, the fetch cap and `src/relevance/episodic.rs` are untouched.
- No old eval card's figures were edited.

## 8. Tests (`src/tests/messages_fts_test.rs`)

| Test | What it pins |
|---|---|
| `migration_installs_the_index_state_and_triggers` | index + state row + 3 triggers exist; `complete = 0` before a backfill; replaying the migration SQL does not clear a completed index |
| `triggers_keep_the_index_in_sync_on_insert_update_and_delete` | insert indexes, `UPDATE OF content` replaces (no duplicate), delete removes |
| `mid_word_substring_parity_between_like_and_the_prefilter` | `rown` finds `brown`, and the prefiltered set equals the plain set |
| `a_term_shorter_than_one_trigram_falls_back_to_like` | `for_phrase`/`for_tokens` return `None` for a 2-char term while the index is READY, and the LIKE fallback still finds it |
| `the_prefilter_changes_the_plan_not_the_result_set` | 13 hostile queries, prefiltered vs plain, identical rows and order |
| `the_track_a_harness_and_the_shipped_search_agree` | `search_slice` and `search_by_content_union` return the same ids in the same order (#911) |
| `the_prefilter_clause_binds_one_parameter_and_scopes_by_rowid` | the shared constant binds one parameter and stays a `rowid IN` sub-select |
