# ADR 0001 compliance report

**ADR:** `docs/adr/0001-telegram-local-file-and-image-delivery.md` (revision 5)
**Binary under test:** opencrabs 0.5.4 @ bf74d9e93317124c5a33978951488d17dac45e51
**Evidence recorded at:** 2026-11-13T18:27:52Z
**Since then:** docs-only, measured: `git diff --name-only bf74d9e93..25e83b5c4 -- src/` is EMPTY, and the three paths that changed are docs/adr/probes/{COMPLIANCE.md,live-evidence.json,manifest.json}. The box runs opencrabs 0.5.4 @ 25e83b5c4, whose compiled code is identical to the binary these observations were taken on, so the verdicts are not stale in the sense the caveat warns about.
**Runner:** `python3 docs/adr/probes/run.py` — **exit 0**
**21/21 passed · 0 failed · 0 missing (missing counts as failed)**

> **GENERATED FILE — do not hand-edit.** Written by `python3 docs/adr/probes/run.py --report`
> from `manifest.json` + `live-evidence.json` in the same run that printed these counts, so a
> number here cannot drift from the probe that produced it. Re-generate after any probe or
> evidence change; a stale page is a false claim, not a rounding error.

## Rule → probe coverage

| ADR rule | Probe | Kind | State |
|---|---|---|---|
| §2 rule 1 | `r1-image-entry-by-magic-bytes` | static | PASS |
| §2 rule 1 | `r1-image-kind-not-extension` | static | PASS |
| §2 rule 2 | `r2-video-entry-by-head-walk` | static | PASS |
| §2 rule 3 | `r3-document-producer-exists` | static | PASS |
| §2 rule 4 | `r4-md-exception-deleted` | static | PASS |
| §2 rule 5 | `r5-detached-floor-exists` | static | PASS |
| §2 rule 6 | `r6-document-emit-is-link` | static | PASS |
| §2 rule 6 | `r6-photo-and-video-emit-embed` | static | PASS |
| §2 rule 6 | `r6-both-forms-read` | static | PASS |
| §3.3 | `s33-orphan-reference-neutralised` | static | PASS |
| §3.4 | `s34-refused-entry-costs-one-entry` | static | PASS |
| §3.5 | `s35-one-reply-one-delivery` | static | PASS |
| §2 rules 3 and 6 | `live-l1-document-link-renders` | live | PASS |
| §2 rule 6, #825 | `live-l2-remote-link-not-fetched` | live | PASS |
| §3.1 | `live-l3-bare-path-inert` | live | PASS |
| §2 rule 1 | `live-l4-remote-image-embed-is-a-photo` | live | PASS |
| §3.5, #826 | `live-l5-one-bubble-per-reply` | live | PASS |
| §2 rule 1 (probe input) | `fixture-l6-local-photo-valid` | fixture | PASS |
| §2 rule 1 | `live-l6-local-image-embed-is-a-photo` | live | PASS |
| §11 | `ci-gate` | ci | PASS |
| §2 rule 5 | `live-l7-tool-plane-local-image-detached-floor` | live | PASS |

## Live probes — the wire as Telegram stored it

### `live-l1-document-link-renders` — §2 rules 3 and 6 → **PASS**

- **Claim under test:** a local document sent through the daemon is stored as a link-form tg://document reference and lands as one bubble
- **tg_get_messages** (msg `96084`, chat `-1003936827469`): [827-smoke.md](tg://document?id=doc0)
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T10:19:48Z
- **Probe:** one local .md referenced in link syntax, nothing else
- **log:** `10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage chat=-1003936827469`
- **log:** `10:19:50.235 delivery.rs:491 Telegram dedup: match found among 1 intermediates (normalized) — suppressing final response`
- **Analysis:** §2 rule 3 + rule 6: the document plane emits LINK syntax and the reply lands as ONE bubble. The owner opened a link-form document bubble by tapping it (2026-11-12 12:01); the embed-form twin (18:33) was not tappable. That client-side half is the owner's observation, not mine.

### `live-l2-remote-link-not-fetched` — §2 rule 6, #825 → **PASS**

- **Claim under test:** a remote URL written in link syntax stays a hyperlink: no fetch, no unsupported-format trailer
- **tg_get_messages** (msg `96031`, chat `-1003936827469`): [#823](https://github.com/leshchenko1979/opencrabs/issues/823)
- **Bubbles observed:** None · **sent at:** 2026-11-12T15:04:46Z
- **Probe:** a remote URL written as [label](https://...)
- **log:** `no 'not a supported image format' line after the 15:03 swap; the only two hits that day quote PRE-fix messages inside reply-context lines`
- **Analysis:** #825 holds: link syntax is never a fetch candidate. Before the fix the same shape produced a ⚠️ 'not a supported image format' trailer.

### `live-l3-bare-path-inert` — §3.1 → **PASS**

- **Claim under test:** an unbracketed path in prose is claimed by no family and stays inert text
- **tg_get_messages** (msg `96054`, chat `-1003936827469`): /tmp/oc-probes/probe-bare.txt
- **Bubbles observed:** 1 · **sent at:** 2026-11-12T18:33:02Z
- **Probe:** an unbracketed absolute path in prose
- **log:** `18:33:02.702 Telegram send ok: kind=rich_api (the same body that carried six document refs)`
- **Analysis:** §3.1: no walk claims an unbracketed path — the text stayed inert, no marker, no attachment.

### `live-l4-remote-image-embed-is-a-photo` — §2 rule 1 → **PASS**

- **Claim under test:** a REMOTE image reference written in embed syntax is fetched and arrives as a photo
- **tg_get_messages** (msg `96055`, chat `-1003936827469`): photo, 7462 bytes, image/jpeg
- **Bubbles observed:** 1 · **sent at:** 2026-11-12T18:33:03Z
- **Probe:** a REMOTE image referenced in embed syntax
- **log:** `18:33:03.338 Telegram send ok: kind=delivery_media path=image_photo`
- **Analysis:** §2 rule 1 for the remote case: fetched and attached as a photo in its own bubble.

### `live-l5-one-bubble-per-reply` — §3.5, #826 → **PASS**

- **Claim under test:** a reply carrying documents lands as one bubble, with no degraded HTML copy beside it
- **tg_get_messages** (msg `96139`, chat `-1003936827469`): ONE rich bubble (rich=true), six document references in the body, no second bubble after it
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T18:25:06Z
- **Probe:** one reply carrying SIX local document references, embed syntax (§3.5 / #826)
- **log:** `18:25:06.875167 Telegram send ok: origin=tool detail=send kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96139 len=691 hash8=85a30967`
- **log:** `absence sweep 18:25:00-18:26:30 chat 3936827469 for 'send failed|html_or_plain|degrad|fallback' -> EMPTY`
- **log:** `positive control, same sweep, old binary 2026-11-12T18:33:01-03: 'Telegram send failed: kind=rich_api path=sendRichMessage' -> 'rich markdown+media send failed (400 Bad Request: IMAGE_PROCESS_FAILED); falling back' -> 'kind=html_or_plain path=html msg=96056' (CAUGHT)`
- **Analysis:** §3.5 re-probed on the swapped binary bf74d9e93. The old observation (msg 96056) was a degraded HTML copy landing beside the retried rich reply; that bubble is absent now, and the sweep that would have caught it is proven live by the positive control above.

### `live-l6-local-image-embed-is-a-photo` — §2 rule 1 → **PASS**

- **Claim under test:** a LOCAL image reference written in embed syntax is attached as a photo; the measured failure is that its bytes are refused and the picture never arrives
- **tg_get_messages** (msg `96148`, chat `-1003936827469`): ONE rich message (rich=true) whose media array carries the photo: rich_kind=photo, rich_media_id=5456183027160524023, image/jpeg, 22568 bytes; the body carries the inline reference [media: photo 5456183027160524023] and the caption as the markdown title
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T18:55:29Z
- **Probe:** a LOCAL 5180-byte 512x512 PNG referenced in embed syntax, delivered on the TURN plane (the rich path) — §2 rule 1 local case
- **log:** `18:55:2x Telegram rich verdict: true (rich_messages=true, structured=false, media_forced=true, table=false, len=742)  [rich/detect.rs:112]`
- **log:** `18:55:29 Telegram send ok: origin=turn detail=- session=- kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96148 len=742 hash8=28cbe125`
- **Analysis:** §2 rule 1 local case, measured on the plane the rule NAMES. Rule 1 requires the image bytes to ride the rich message's media array; msg 96148 is exactly that — a rich=true message whose media array carries rich_media_id 5456183027160524023 as a photo, with the reference inline in the body. CORRECTION, recorded rather than quietly overwritten: this probe was first filed citing msg 96143, which is the TOOL plane's detached upload (telegram_send → send.rs, which never consults rich_messages). That observation measures rule 5's floor and is now recorded separately as L7. The 2026-11-12 withdrawal of this probe was for a truncated FIXTURE, not a binary defect; the fixture is structurally validated (fixture-l6 PASS: 512x512, CRCs OK, ends on IEND, IDAT inflates to 1049088 = expected).

### `live-l7-tool-plane-local-image-detached-floor` — §2 rule 5 → **PASS**

- **Claim under test:** with no rich path the local image's bytes still ship, as their own detached bubble (the floor), rather than being held back
- **tg_get_messages** (msg `96143`, chat `-1003936827469`): TWO bubbles: a text chunk (msg 96142, html, len 218) and a detached photo bubble (msg 96143, media.type=photo, image/jpeg, 22568 bytes)
- **Bubbles observed:** 2 · **sent at:** 2026-11-13T18:27:52Z
- **Probe:** a LOCAL 5180-byte 512x512 PNG sent through the TOOL plane (telegram_send), which has no rich path at all — §2 rule 5 detached floor
- **log:** `18:27:52.216350 Telegram send ok: origin=tool detail=send kind=html_or_plain path=html chat=-1003936827469 thread=Some(31847) msg=96142 len=218 hash8=4f7c5689`
- **log:** `18:27:52.725532 Telegram send ok: origin=tool detail=send kind=outbox path=image_photo chat=-1003936827469 thread=Some(31847) msg=96143 len=5180 hash8=697081ce`
- **Analysis:** §2 rule 5: with no rich path, every kind ships as its own detached bubble. The tool plane IS that case by construction — send.rs never reads rich_messages, so no rich path exists there — and it is measured: the bytes ship as a detached photo bubble beside an html text chunk, with len=5180 (the fixture's exact byte count) proving which file rode. This is the floor, not the rich plane; it is recorded here so rule 5 has a LIVE receipt, and so L6 cannot be read as measuring it.

## Section 11 — the tree compiles and its tests pass

`ci-gate` → **PASS** · run **43466381479** on `leshchenko1979/opencrabs` · run 43466381479: completed success

green gate run for bf74d9e93317124c5a33978951488d17dac45e51 (job 'gate / PR-lane gates (bf74d9e93...)', conclusion success, updatedAt 2026-11-13T17:21:46Z)

`completed success`

## What this instrument cannot see

- **Client rendering.** `tg_get_messages` returns what the API stored; whether a client *draws* a
  reference is a different instrument (#827 was exactly that gap — the API accepted a form the client
  would not open). A row marked PASS here means the wire is right, not that a human eye confirmed it.
- **Stale evidence.** An observation recorded before a swap says nothing about the binary after it.
  Every live row above carries its `sent_at`, and the `binary` line at the top names what was running
  when the evidence was written. Re-probe after a swap; a PASS carried across one is not a measurement.
- **Load behaviour.** One probe, one message. A rate-limited or partially-failing send is not exercised.
- **Which PLANE a verdict speaks for.** §2 rule 1 (a resolvable image arrives as a photo, its bytes in
  the rich message's `media` array) and §2 rule 5 (with no rich path, the bytes ship as their own
  detached bubble — the floor) are DIFFERENT planes, and a probe only measures the one it travelled.
  The local-image rule 1 probe rides the turn plane (`delivery.rs`); the tool plane (`telegram_send`
  → `send.rs`) has no rich path at all and is recorded separately as the rule 5 floor. Reading either
  one as evidence for the other is how a suite certifies a defect.


## Reproduce

```
cd <repo> && python3 docs/adr/probes/run.py            # verdicts, exit 0 iff all pass
cd <repo> && python3 docs/adr/probes/run.py --report docs/adr/probes/COMPLIANCE.md
```
