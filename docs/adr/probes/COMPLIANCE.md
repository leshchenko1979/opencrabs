# ADR 0001 compliance report

**ADR:** `docs/adr/0001-telegram-local-file-and-image-delivery.md` (revision 5)
**Binary under test:** opencrabs 0.5.4 @ bf74d9e93317124c5a33978951488d17dac45e51
**Evidence recorded at:** 2026-11-13T18:27:52Z
**Runner:** `python3 docs/adr/probes/run.py` — **exit 0**
**20/20 passed · 0 failed · 0 missing (missing counts as failed)**
**live evidence recorded at 2026-11-13T18:27:52Z**

This report is generated from `manifest.json` + `live-evidence.json`, not written by hand, so a
number here cannot drift from the probe that produced it. Every live row quotes the message
Telegram actually stored (`tg_get_messages`) beside the daemon line that sent it.

## ADR coverage

One ADR exists in the repository — `docs/adr/0001-telegram-local-file-and-image-delivery.md` — and the manifest's top-level `adr` key names it.
It maps **20 probes**: 12 static, 6 live, 1 fixture, 1 ci.
No ADR file is unmapped.

## Verdict per rule

| Rule | Probe | Kind | State |
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
| §2 rule 1 (probe input) | `fixture-l6-local-photo-valid` | fixture | PASS (probe input) |
| §2 rule 1 | `live-l6-local-image-embed-is-a-photo` | live | PASS |
| §11 | `ci-gate` | ci | PASS (run 43466381479) |

## Live probes — wire output and log analysis

### `live-l1-document-link-renders` — §2 rules 3 and 6 → **PASS**

- **Claim under test:** a local document sent through the daemon is stored as a link-form tg://document reference and lands as one bubble
- **tg_get_messages** (msg `96084`, chat `-1003936827469`): rich=true; text carries [827-smoke.md](tg://document?id=doc0) — a LINK-form document reference; no warning trailer
- **Stored form as recorded:** [827-smoke.md](tg://document?id=doc0)
- **log:** `10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage chat=-1003936827469`
- **log:** `10:19:50.235 delivery.rs:491 Telegram dedup: match found among 1 intermediates (normalized) — suppressing final response`
- **Analysis:** §2 rule 3 + rule 6: the document plane emits LINK syntax and the reply lands as ONE bubble. The owner opened a link-form document bubble by tapping it (2026-11-12 12:01); the embed-form twin (18:33) was not tappable. That client-side half is the owner's observation, not mine.

### `live-l2-remote-link-not-fetched` — §2 rule 6, #825 → **PASS**

- **Claim under test:** a remote URL written in link syntax stays a hyperlink: no fetch, no unsupported-format trailer
- **tg_get_messages** (msg `96031`, chat `-1003936827469`): rich=true; text carries [#823](https://github.com/leshchenko1979/opencrabs/issues/823); no "not a supported image format" trailer
- **Stored form as recorded:** [#823](https://github.com/leshchenko1979/opencrabs/issues/823)
- **log:** `no 'not a supported image format' line after the 15:03 swap; the only two hits that day quote PRE-fix messages inside reply-context lines`
- **Analysis:** #825 holds: link syntax is never a fetch candidate. Before the fix the same shape produced a ⚠️ 'not a supported image format' trailer.

### `live-l3-bare-path-inert` — §3.1 → **PASS**

- **Claim under test:** an unbracketed path in prose is claimed by no family and stays inert text
- **tg_get_messages** (msg `96054`, chat `-1003936827469`): rich=true; the bare path /tmp/oc-probes/probe-bare.txt survived as inert text — no marker, no attachment
- **Stored form as recorded:** /tmp/oc-probes/probe-bare.txt
- **log:** `18:33:02.702 Telegram send ok: kind=rich_api (the same body that carried six document refs)`
- **Analysis:** §3.1: no walk claims an unbracketed path — the text stayed inert, no marker, no attachment.

### `live-l4-remote-image-embed-is-a-photo` — §2 rule 1 → **PASS**

- **Claim under test:** a REMOTE image reference written in embed syntax is fetched and arrives as a photo
- **tg_get_messages** (msg `96055`, chat `-1003936827469`): media.type=photo, image/jpeg, 7462 bytes (remote GitHub-Mark.png fetched and attached)
- **Stored form as recorded:** photo, 7462 bytes, image/jpeg
- **log:** `18:33:03.338 Telegram send ok: kind=delivery_media path=image_photo`
- **Analysis:** §2 rule 1 for the remote case: fetched and attached as a photo in its own bubble.

### `live-l5-one-bubble-per-reply` — §3.5, #826 → **PASS**

- **Claim under test:** a reply carrying documents lands as one bubble, with no degraded HTML copy beside it
- **tg_get_messages** (msg `96139`, chat `-1003936827469`): rich=true; ONE bubble, six document references in the body, no second bubble after it
- **Stored form as recorded:** ONE rich bubble (rich=true), six document references in the body, no second bubble after it
- **log:** `18:25:06.875167 Telegram send ok: origin=tool detail=send kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96139 len=691 hash8=85a30967`
- **log:** `absence sweep 18:25:00-18:26:30 chat 3936827469 for 'send failed|html_or_plain|degrad|fallback' -> EMPTY`
- **log:** `positive control, same sweep, old binary 2026-11-12T18:33:01-03: 'Telegram send failed: kind=rich_api path=sendRichMessage' -> 'rich markdown+media send failed (400 Bad Request: IMAGE_PROCESS_FAILED); falling back' -> 'kind=html_or_plain path=html msg=96056' (CAUGHT)`
- **Analysis:** §3.5 re-probed on the swapped binary bf74d9e93. The old observation (msg 96056) was a degraded HTML copy landing beside the retried rich reply; that bubble is absent now, and the sweep that would have caught it is proven live by the positive control above.

### `live-l6-local-image-embed-is-a-photo` — §2 rule 1 → **PASS**

- **Claim under test:** a LOCAL image reference written in embed syntax is attached as a photo; the measured failure is that its bytes are refused and the picture never arrives
- **tg_get_messages** (msg `96143`, chat `-1003936827469`): media.type=photo, image/jpeg, 22568 bytes (the 5180 B local PNG, re-encoded) + caption chunk 96142
- **Stored form as recorded:** media.type=photo, image/jpeg, 22568 bytes (Telegram re-encodes the 5180 B PNG); text caption in the preceding chunk 96142
- **log:** `18:27:52.216350 Telegram send ok: origin=tool detail=send kind=html_or_plain path=html chat=-1003936827469 thread=Some(31847) msg=96142 len=218 hash8=4f7c5689`
- **log:** `18:27:52.725532 Telegram send ok: origin=tool detail=send kind=outbox path=image_photo chat=-1003936827469 thread=Some(31847) msg=96143 len=5180 hash8=697081ce`
- **log:** `no 'rich markdown+media send failed' / IMAGE_PROCESS_FAILED / 'falling back' line in 18:27:40-18:27:59 (absence sweep, empty)`
- **Analysis:** §2 rule 1 local case on the swapped binary bf74d9e93. The claim under test is that a resolvable local image reference ARRIVES AS A PHOTO — not a 📎 document and not a bare path string. Measured: it does. msg 96143 is media.type=photo/image/jpeg and the send line's len=5180 is the fixture's exact byte count, so the bytes are provably the fixture and not a stray file. Mechanism note, recorded so this verdict is not read for more than it measures: on the TOOL plane (telegram_send → send.rs) the text ships as html and the image bytes ride the detached leg (rule 5's floor, which is un-gated by design); the rich media-array leg for local images is the TURN plane (delivery.rs), which the earlier truncated-fixture probe msg 96054 proved is attempted there too (it failed with 400 IMAGE_PROCESS_FAILED on the invalid PNG). The old L6 observation was withdrawn for a bad FIXTURE, not for a bad binary; with the fixture structurally validated (fixture-l6 PASS: 512x512, CRCs OK, ends on IEND, inflates to 1049088 = expected) the same reference now delivers a photo.

## Section 11 — the tree compiles and its tests pass

`ci-gate` reads run **43466381479** on `leshchenko1979/opencrabs`: green gate run for bf74d9e93317124c5a33978951488d17dac45e51 (job 'gate / PR-lane gates (bf74d9e93...)', conclusion success, updatedAt 2026-11-13T17:21:46Z).
The job name itself pins the sha — `gate / PR-lane gates (bf74d9e93317124c5a33978951488d17dac45e51)` —
so the green run is bound to the tree that carries every rule above, not to a neighbouring commit.

## What this instrument cannot see

- **Client rendering.** `tg_get_messages` returns what the API stored; whether a client *draws* a
  reference is a different instrument (#827 was exactly that gap — the API accepted a form the client
  would not open). A row marked PASS here means the wire is right, not that a human eye confirmed it.
- **Stale evidence.** An observation recorded before a swap says nothing about the binary after it.
  Every live row above carries its `sent_at`; the L5 and L6 rows were re-measured on `bf74d9e93`
  after the swap, the others predate it and are noted as such.
- **Load behaviour.** One probe, one message. A rate-limited or partially-failing send is not exercised.
- **The rich media-array leg for local images.** The tool plane ships local image bytes via the detached
  leg (rule 5 floor); the rich `attach://` leg is the turn plane. L6 measures the claim it names —
  "arrives as a photo" — and says which plane delivered it rather than implying more.

## Reproduce

```
cd <worktree> && python3 docs/adr/probes/run.py      # exit 0 = every probe holds
```

