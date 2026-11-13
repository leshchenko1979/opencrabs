# ADR 0001 compliance report

**ADR:** `docs/adr/0001-telegram-local-file-and-image-delivery.md` (revision 5)
**Generated:** 2026-11-13, against `fix/823-md-inline` @ `f85b98c37`
**Instrument:** `python3 docs/adr/probes/run.py` (manifest: `docs/adr/probes/manifest.json`, live evidence: `docs/adr/probes/live-evidence.json`)

Every normative rule in the ADR maps to at least one executable probe. Nothing in this
report is a restatement of intent: a `static` probe is a command that exits 0, a `live`
probe is a message read back with `tg_get_messages` plus the daemon log line for the same
instant, and the `ci` probe is a GitHub Actions conclusion read from the API.

## Verdict

**16 / 19 pass · 2 measured violations · 1 awaiting a run id.**

The two violations are the honest part of this report. Both are recorded as failing
probes rather than edited out, because a manifest that cannot fail is not an instrument.

| # | Rule | Probe | State |
|---|---|---|---|
| 1 | §2 rule 1 — local image renders inline | `r1-image-entry-by-magic-bytes` (static) | PASS |
| 2 | §2 rule 1 — kind by bytes, not extension | `r1-image-kind-not-extension` (static) | PASS |
| 3 | §2 rule 1 — a local image ARRIVES as a photo | `live-l6-local-image-embed-is-a-photo` | **FAIL** → [#829](https://github.com/leshchenko1979/opencrabs/issues/829) |
| 4 | §2 rule 1 — a remote image arrives as a photo | `live-l4-remote-image-embed-is-a-photo` | PASS |
| 5 | §2 rule 2 — video claimed by box walk | `r2-video-entry-by-head-walk` (static) | PASS |
| 6 | §2 rule 3 — document producer exists | `r3-document-producer-exists` (static) | PASS |
| 7 | §2 rule 3+6 — a document lands as a link-form reference | `live-l1-document-link-renders` | PASS |
| 8 | §2 rule 4 — `.md` exception deleted | `r4-md-exception-deleted` (static) | PASS |
| 9 | §2 rule 5 — detached floor still exists | `r5-detached-floor-exists` (static) | PASS |
| 10 | §2 rule 6 — document emits LINK syntax | `r6-document-emit-is-link` (static) | PASS |
| 11 | §2 rule 6 — photo/video emit image syntax | `r6-photo-and-video-emit-embed` (static) | PASS |
| 12 | §2 rule 6 — both markdown forms are read | `r6-both-forms-read` (static) | PASS |
| 13 | §2 rule 6 (#825) — a remote link is never fetched | `live-l2-remote-link-not-fetched` | PASS |
| 14 | §3.1 — an unbracketed path is inert | `live-l3-bare-path-inert` | PASS |
| 15 | §3.3 — an unanswerable reference is neutralised | `s33-orphan-reference-neutralised` (static) | PASS |
| 16 | §3.4 — a refused entry costs one entry | `s34-refused-entry-costs-one-entry` (static) | PASS |
| 17 | §3.5 — one reply, one delivery | `s35-one-reply-one-delivery` (static) | PASS |
| 18 | §3.5 (#826) — a reply lands as ONE bubble | `live-l5-one-bubble-per-reply` | **FAIL** on the shipped binary, fixed in tree |
| 19 | §11 — the tree compiles and passes tests | `ci-gate` | pending run id |

## The two measured violations

### §2 rule 1, local image — [#829](https://github.com/leshchenko1979/opencrabs/issues/829)

The ADR says a resolvable local image renders inline. Measured on the live binary
`db7d3b603`, a valid 116-byte 16×16 PNG produced a `tg://photo?id=img0` reference and no
picture:

```
18:33:01.819  Telegram send failed: kind=rich_api (400 Bad Request: IMAGE_PROCESS_FAILED)
stored 96054: ![probe-embed.png](tg://photo?id=img0)     ← reference, no photo behind it
$ file /tmp/oc-probes/probe-embed.png
PNG image data, 16 x 16, 8-bit/color RGBA, non-interlaced
```

The remote image in the same reply attached fine (`96055`, 7 462 B, `image/jpeg`), so the
fetch path works and the local-attachment path does not. Filed, not fixed — the cause is
not yet measured and guessing one would be the same mistake this report exists to avoid.

### §3.5, one bubble per reply — [#826](https://github.com/leshchenko1979/opencrabs/issues/826)

Two bubbles, 0.9 s apart, from one reply:

```
18:33:02.702  send ok: kind=rich_api      msg=96054   (six documents, link form)
18:33:03.583  send ok: kind=html_or_plain msg=96056   (six 📎, pictures gone)
```

The rich leg failed on `IMAGE_PROCESS_FAILED`, its retry landed as `96054`, and the
degraded HTML copy shipped beside it. **Fixed in this branch** (`bca4cedf5`): the
suppression arm compares the reply with reference spelling erased, so
`[report.pdf](/tmp/x.pdf)`, `[report.pdf](tg://document?id=doc0)` and `📎 report.pdf` are
the same reply. **Not yet re-probed**, because the fix is unshipped — this verdict stays
FAIL until the new binary is live and the same ten-file probe is read back.

## Live evidence, quoted

Each observation is the API's own stored form, read with `tg_get_messages` on
chat `-1003936827469`, paired with the daemon log line from the same second.

**L1 — a document, link form, one bubble** (`96084`, 2026-11-13T10:19:48Z)

```
stored:  [827-smoke.md](tg://document?id=doc0)
log:     10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage
log:     10:19:50.235 delivery.rs:491 dedup: match found among 1 intermediates — suppressing final response
```

The reference is LINK syntax, exactly the form the owner opened by tapping; and the dedup
matched, so no second bubble followed. This is the shape §2 rule 6 now mandates.

**L2 — a remote hyperlink is not fetched** (`96031`, 2026-11-12T15:04:46Z)

```
stored:  [#823](https://github.com/leshchenko1979/opencrabs/issues/823)
trailer: absent
```

The only two `not a supported image format` log hits that day quote pre-fix messages
inside reply-context lines. #825 holds.

**L3 — an unbracketed path is inert** (`96054`)

```
stored:  /tmp/oc-probes/probe-bare.txt
```

No marker, no attachment, no entry — §3.1 is satisfied by omission, which is the rule.

**L4 — a remote image is a photo** (`96055`)

```
stored:  photo, 7462 bytes, image/jpeg
log:     18:33:03.338 Telegram send ok: kind=delivery_media path=image_photo
```

**L5 — the duplicate** (`96054` + `96056`) — quoted above.

**L6 — the refused local image** (`96054`) — quoted above.

## What this instrument cannot see

- **Whether the client DRAWS what the API accepted.** The #827 lesson: the API accepted
  the embed form for a document and the client rendered nothing. Every `live` probe here
  reads the API's stored form; only the owner's tap settles rendering. That is why §2
  rule 6 rests on his measurement, not mine.
- **A stale evidence file.** `live-evidence.json` carries `recorded_at`; the runner prints
  it. An observation recorded before a ship says nothing about the binary after it.
- **Behaviour under load.** The probes are one reply at a time; the `429` / `IMAGE_PROCESS_FAILED`
  interactions in #826 needed a ten-file reply to surface.

## Reproduce

```bash
cd <repo> && python3 docs/adr/probes/run.py            # exit 0 iff every probe passes
python3 docs/adr/probes/run.py --allow-missing --json /tmp/adr-probes.json
```

`--allow-missing` reports an unrecorded observation without failing, for a pre-ship run.
