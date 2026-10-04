# `rebuild-tool` — maintainer-local `/rebuild` patch

The `/rebuild` command and its `rebuild` tool were **removed from the public tree** (issue #674) because they are maintainer tooling, not a product feature: the tool compiles OpenCrabs' own source, auto-clones a source tree on first use, and exec-restarts the process. This directory preserves the capability as a **patch** so a maintainer or contributor who wants a local rebuild can put it back on their own checkout.

It is deliberately a patch and not live code. Keeping the tool as a standing fork commit would leave a permanent divergence in `catalog.rs`, `slash_command.rs`, `cli/ui.rs` and `tui/app/state.rs`, so every upstream sync would risk a conflict forever. A patch costs nothing to carry and is applied only when wanted.

## When to apply it

You are building OpenCrabs **from source on your own machine** and want the agent to be able to rebuild itself (`/rebuild`, or the `rebuild` tool). If you install pre-built releases, you do not need this — use `/evolve`.

## How to apply it

From a clean checkout of the public repository (the removal has landed):

```sh
git apply patches/rebuild-tool/0001-restore-rebuild-tool.patch
cargo build --release
```

The patch touches 38 files and is purely additive against the post-removal tree: it restores the tool implementation, its module declaration, its catalog entry, its CLI registration, the `/rebuild` slash command and help line, the TUI command-list entry, the README rows and the three shipped brain templates.

To back it out:

```sh
git apply -R patches/rebuild-tool/0001-restore-rebuild-tool.patch
```

## What it restores — and what it does not

Restores everything the removal took: `src/brain/tools/rebuild.rs`, `pub mod rebuild;`, the catalog entry and prompt guidance, `RebuildTool::new()` in `src/cli/ui.rs`, the five `slash_command.rs` sites, the TUI surface, `README.md`, and `src/docs/reference/templates/{BOOT,CODE,TOOLS}.md`.

It does **not** restore the old cron machinery (`REBUILD_JOB_NAME`, `schedule_background_rebuild`, `run_rebuild_job`). That had already been removed upstream by #1748, which rewrote the tool to run the build as a detached command through the shared `BackgroundTaskManager` — the same path every long-running shell command uses. The `rebuild.rs` this patch restores is that rewritten version, so it is self-contained and needs nothing from the cron layer.

## Provenance

Derived mechanically, not hand-written:

| | |
|---|---|
| Removal commits on the fork | `07d9a6a30` (32 files), `9bc0d0564` (5), `4b3765dc4` (2) |
| Removal base | `9ef10b431` |
| Patch construction | `git diff -R 9ef10b431 4b3765dc4 -- <the 38 files the removal touched>` |
| Verified | `git apply --check` returns 0 against the post-removal tree; after applying, all 38 files are byte-identical to `9ef10b431` |

Regenerate it after any further change to the removal with the same command.

> Note: the patch's context is the **post-removal** tree, because that is the tree a maintainer will have. Applying it to a pre-removal checkout fails — the tool is already there.
