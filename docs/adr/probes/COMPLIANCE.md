# ADR 0001 compliance report

**ADR:** `docs/adr/0001-telegram-local-file-and-image-delivery.md` (revision 6)
**Binary under test:** opencrabs 0.5.4 @ bf74d9e93317124c5a33978951488d17dac45e51
**Evidence recorded at:** 2026-11-14T20:48:00Z
**Since then:** docs-only, measured: `git diff --name-only bf74d9e93..25e83b5c4 -- src/` is EMPTY, and the three paths that changed are docs/adr/probes/{COMPLIANCE.md,live-evidence.json,manifest.json}. The box runs opencrabs 0.5.4 @ 25e83b5c4, whose compiled code is identical to the binary these observations were taken on, so the verdicts are not stale in the sense the caveat warns about.
**Runner:** `python3 docs/adr/probes/run.py` — **exit 1**
**33/41 passed · 8 failed · 0 missing (missing counts as failed)**

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
| §3.2 | `s21-mixed-kind-namespace` | static | PASS |
| §3.2 caption | `s22-caption-gate` | static | PASS |
| §3.5, #826 | `s23-dedup-canonical-path` | static | PASS |
| §2 rule 1 floor | `s24-oversize-split-floor` | static | PASS |
| §2 rule 2 floor | `s25-video-kind-floor` | static | PASS |
| §6 | `s36-thread-id-conditional` | static | PASS |
| §2 rules 3 and 6 | `live-l1-document-link-renders` | live | FAIL |
| §2 rule 6, #825 | `live-l2-remote-link-not-fetched` | live | PASS |
| §3.1 | `live-l3-bare-path-inert` | live | PASS |
| §2 rule 1 | `live-l4-remote-image-embed-is-a-photo` | live | PASS |
| §3.5, #826 | `live-l5-one-bubble-per-reply` | live | PASS |
| §2 rule 1 (probe input) | `fixture-l6-local-photo-valid` | fixture | PASS |
| §3.4 (probe input) | `fixture-corrupt-photo-refusable` | fixture-negative | PASS |
| §2 rule 2 (probe input) | `fixture-video-64-valid` | fixture | PASS |
| §2 rule 1 floor (probe input) | `fixture-oversize-photo-valid` | fixture | PASS |
| §3.2 caption (probe input) | `fixture-caption-ok-valid` | fixture | PASS |
| §3.2 caption (probe input) | `fixture-caption-bad-valid` | fixture | PASS |
| §2 rule 1 | `live-l6-local-image-embed-is-a-photo` | live | PASS |
| §11 | `ci-gate` | ci | PASS |
| §2 rule 5 | `live-l7-tool-plane-local-image-detached-floor` | live | PASS |
| §2 rule 6 | `live-l8-local-image-link-syntax-attaches` | live | FAIL |
| §3.2 | `live-l9-mixed-kinds-one-message` | live | FAIL |
| §2 rule 6, #827 | `live-l10-document-embed-demoted-to-link` | live | FAIL |
| §2 rule 2 | `live-l11-video-embed-is-a-video` | live | FAIL |
| §3.2 caption | `live-l12-caption-rides-the-title` | live | PASS |
| §3.2 caption | `live-l13-inexpressible-caption-dropped` | live | FAIL |
| §3.5, #826 | `live-l14-dedup-two-spellings-one-file` | live | FAIL |
| §3.4, #826 | `live-l15-refused-entry-costs-one-entry` | live | PASS |
| §2 rule 1 floor | `live-l16-oversize-image-ships-as-document` | live | FAIL |

## Live probes — the wire as Telegram stored it

### `live-l1-document-link-renders` — §2 rules 3 and 6 → **FAIL**

- **Claim under test:** a local document sent through the daemon is stored as a link-form tg://document reference and lands as one bubble
- **tg_get_messages** (msg `96084`, chat `-1003936827469`): [827-smoke.md](tg://document?id=doc0)  — LINK syntax in the body, and NOTHING behind it
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T10:19:48Z
- **Probe:** one local .md referenced in link syntax, nothing else
- **log:** `10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage chat=-1003936827469`
- **log:** `10:19:50.235 delivery.rs:491 Telegram dedup: match found among 1 intermediates (normalized) — suppressing final response`
- **log:** `raw-TL read 2026-11-14: documents: [] , photos: [] , media: null  (tg_get_messages had reported the same message as carrying a document reference)`
- **Analysis:** CORRECTED 2026-11-14 against raw MTProto (messages.GetHistory, peer -1003936827469). The FORM half of the probe holds: the document plane emits LINK syntax and the reply is one bubble. The ATTACHMENT half fails: the stored message carries documents: [] and photos: [], so the tg://document?id=doc0 reference answers to no entry. The earlier PASS was read through tg_get_messages, which materialises photos only and reports documents: [] for EVERY message — a blind instrument cannot falsify an attachment claim. §2 rule 3 requires the file to inline as a document media entry in the same message; this message has no entry, so rule 3 is not met. The client-tap observation that motivated #827 (2026-11-12 12:01) therefore did not observe an attached document on THIS message, and cannot be used to justify the link form on its own.

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
- **tg_get_messages** (msg `96139`, chat `-1003936827469`): ONE rich bubble (rich=true), and — correction — ZERO tg:// references in it: six PageListOrderedItemText plain labels, documents: [], photos: []
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T18:25:06Z
- **Probe:** one reply carrying SIX local document references, embed syntax (§3.5 / #826)
- **log:** `18:25:06.875167 Telegram send ok: origin=tool detail=send kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96139 len=691 hash8=85a30967`
- **log:** `absence sweep 18:25:00-18:26:30 chat 3936827469 for 'send failed|html_or_plain|degrad|fallback' -> EMPTY`
- **log:** `positive control, same sweep, old binary 2026-11-12T18:33:01-03: 'Telegram send failed: kind=rich_api path=sendRichMessage' -> 'rich markdown+media send failed (400 Bad Request: IMAGE_PROCESS_FAILED); falling back' -> 'kind=html_or_plain path=html msg=96056' (CAUGHT)`
- **Analysis:** §3.5 re-probed on the swapped binary bf74d9e93. The old observation (msg 96056) was a degraded HTML copy landing beside the retried rich reply; that bubble is absent now, and the sweep that would have caught it is proven live by the positive control above. CORRECTED 2026-11-14 (raw MTProto): this message proves §3.5 (one reply, one bubble) and proves NOTHING about documents — its body carries six plain labels and zero tg:// references, and documents: []. The six files had already been delivered earlier in the conversation, so the already-delivered filter consumed the references silently (ADR §5) and no entry was emitted. The earlier stored_form sentence, 'six document references in the body', was false and is replaced above.

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

### `live-l8-local-image-link-syntax-attaches` — §2 rule 6 → **FAIL**

- **Claim under test:** a LOCAL image referenced in LINK syntax `[label](path)` is still claimed by the image family and arrives as a photo entry — the two markdown forms are equivalent for local targets, which is the property that makes link syntax safe to author
- **tg_get_messages** (msg `96084`, chat `-1003936827469`): [827-smoke.md](tg://document?id=doc0) — a bare TextUrl paragraph; photos [], documents [], media null
- **Bubbles observed:** None · **sent at:** 2026-11-13T10:19:48Z
- **Probe:** a local document referenced in LINK syntax, nothing else in the reply
- **log:** `10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage chat=-1003936827469 msg=96084 len=... (accepted, no error)`
- **Analysis:** §2 rule 6 asks which form the WIRE reads. On this message: link syntax produced NO entry at all. It is the same message L1 cites, read the same way — the difference is that L1 was scored on the body's shape and this leg on whether anything is behind it. The suite's own §4.1 table (2026-10-29) says the same: every `link` row delivered no attachment, every `embed` row delivered.

### `live-l9-mixed-kinds-one-message` — §3.2 → **FAIL**

- **Claim under test:** one reply carrying an image, a video and a document produces THREE distinct entries in ONE rich message, with the `img`/`vid`/`doc` namespaces keeping each reference answered by its own kind
- **tg_get_messages** (msg `96248`, chat `-1003936827469`): ONE rich bubble. Photo attached (PageBlockPhoto, photo 5456183027160524023, 512x512). Video and document did NOT attach: both survive only as TextUrl paragraphs, documents: []
- **Bubbles observed:** 1 · **sent at:** 2026-11-14T19:53:50Z
- **Probe:** one reply carrying three kinds at once: local image + local video + local document (§3.2 namespace split)
- **log:** `19:53:50.732547 kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96248 len=966 hash8=ef3fd4ef`
- **log:** `absence sweep same instant for 'send failed|falling back|degrad|IMAGE_PROCESS_FAILED|failed to read promoted' -> EMPTY (count 0)`
- **Analysis:** §3.2's namespace split is NOT demonstrated: one bubble yes, three entries no. The photo entry bound and the other two did not, and nothing was refused or retried — so the entries were either never sent or dropped in transit without an error. The absence sweep is the load-bearing half: no rejection, no fallback, no sacrifice path ran. This is the leg that motivated L1/L5 being read through a blind instrument.

### `live-l10-document-embed-demoted-to-link` — §2 rule 6, #827 → **FAIL**

- **Claim under test:** a document authored in EMBED syntax is demoted to link syntax at send time (`shield_unresolvable_markdown_images`), so it lands tappable instead of drawing nothing — the #827 failure mode cannot be re-authored from a reply
- **tg_get_messages** (msg `96313`, chat `None`): `🔍 PROBE L10 — ...\n\n!doc-sample.md` — the link target is gone, the label survives with its leading `!`, and NOTHING was attached (media: null)
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:12Z
- **Probe:** a local document referenced in EMBED syntax: `![doc-sample.md](/root/.../fixtures/doc-sample.md)`
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. A local non-image file is neither attached nor linked: the reference degrades to a bare `!name` in the body. §2 rule 3 (any other file → document) is not met on this plane.

### `live-l11-video-embed-is-a-video` — §2 rule 2 → **FAIL**

- **Claim under test:** a local MP4 in embed syntax arrives as a `video` media entry (playable), not as a document and not as a dead reference
- **tg_get_messages** (msg `96248`, chat `-1003936827469`): no video in the stored message: documents [], and the video reference survives only as TextUrl "tg://video?id=vid0"
- **Bubbles observed:** None · **sent at:** 2026-11-14T19:53:50Z
- **Probe:** a local video referenced in embed syntax, in a reply that also carries a photo and a document
- **log:** `19:53:50.732547 kind=rich_api path=sendRichMessage msg=96248 — accepted, no error`
- **Analysis:** §2 rule 2 is not met on this message. The emit is EMBED for video (r6-photo-and-video-emit-embed passes statically), so this is not the link-form artefact L8 records — an embed-syntax video entry still failed to bind beside a photo that did. Two readings remain open and this probe does not separate them: (a) a video entry is dropped when it shares a message with a photo, or (b) the 64-byte fixture was refused. §4.1 measured a 46 848-byte mp4 delivering PageBlockVideo, so (b) is live and must be excluded by re-probing with a full-size mp4 before any code change is attributed.

### `live-l12-caption-rides-the-title` — §3.2 caption → **PASS**

- **Claim under test:** an expressible caption rides the reference's markdown title and arrives as the media caption, which is the only caption channel the rich plane offers
- **tg_get_messages** (msg `96315`, chat `None`): a MessageMediaPhoto (312 B, image/jpeg) whose caption is `L12 caption rides the title` — the title arrived as the caption, verbatim
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:21Z
- **Probe:** `![cap](caption-ok.png "L12 caption rides the title")`
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. The caption leg passes on the tool plane. It says nothing yet about §3.2's rich-plane title handling.

### `live-l13-inexpressible-caption-dropped` — §3.2 caption → **FAIL**

- **Claim under test:** a caption containing `)` is DROPPED rather than emitted malformed: the image still arrives, merely unnamed, and the reference is not mangled
- **tg_get_messages** (msg `96317`, chat `None`): a MessageMediaPhoto (310 B) whose caption is `L13 bad ) paren` — NOT dropped
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:25Z
- **Probe:** `![bad](caption-bad.png "L13 bad ) paren")` — a title containing `)`, which §3.2's caption gate is supposed to drop
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. The claim behind probe `live-l13-inexpressible-caption-dropped` is contradicted here: a quoted markdown title carrying `)` is legal and arrived intact. Either the gate does not fire, or `)` was never the inexpressible character the ADR names. §3.2 must be re-read against the code before the rule is stated as measured.

### `live-l14-dedup-two-spellings-one-file` — §3.5, #826 → **FAIL**

- **Claim under test:** one file referenced twice in one reply under two spellings (`~/…` and `/root/…`) is delivered ONCE: dedup compares the resolved absolute path
- **tg_get_messages** (msg `96319`, chat `None`): TWO separate MessageMediaPhoto bubbles (96319 and 96320), both carrying photo id 5456183027160524023
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:29Z
- **Probe:** the same file under two spellings in one reply: `.../fixtures/local-photo-512.png` and `.../fixtures/../fixtures/local-photo-512.png`
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. §3.5's one-reply-one-delivery is broken on the tool plane: one file, two bubbles. Telegram itself deduplicated the UPLOAD (both bubbles reference one photo id), so the duplicate is the sender's, not the wire's. The dedup filter that consumes an already-delivered file is not applied to two spellings of the same path in one reply.

### `live-l15-refused-entry-costs-one-entry` — §3.4, #826 → **PASS**

- **Claim under test:** a reply whose media plane contains a file Telegram refuses (`IMAGE_PROCESS_FAILED`) still lands as ONE bubble carrying the healthy entries, instead of downgrading the whole reply to the HTML dialect
- **tg_get_messages** (msg `96322`, chat `None`): the healthy photo landed as MessageMediaPhoto 96322 (312 B); the corrupt one is absent; the reply was not degraded to the HTML dialect
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:36Z
- **Probe:** a reply carrying a corrupt PNG (Telegram refuses it) beside a healthy PNG
- **log:** `20:42:32.723312 ERROR tool/send: failed to send image .../corrupt-photo.png as photo: Bad Request: IMAGE_PROCESS_FAILED`
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. §3.4's #826 AC-3 shape holds on the tool plane: the refusal cost one entry, not the plane. This is the tool-plane twin of the rich-plane leg, not a substitute for it.

### `live-l16-oversize-image-ships-as-document` — §2 rule 1 floor → **FAIL**

- **Claim under test:** a structurally valid 12.01 MiB image is above the `sendPhoto` ceiling, so the floor ships it as a document: it arrives as a file rather than being refused
- **tg_get_messages** (msg `96323`, chat `None`): NOTHING delivered: 96323 is the header text alone, media: null
- **Bubbles observed:** None · **sent at:** 2026-11-14T20:42:39Z
- **Probe:** a structurally valid 12.01 MiB PNG, past the 10 MiB sendPhoto ceiling
- **log:** `20:42:47.298711 ERROR tool/send: failed to send image /tmp/oc-probes/oversize-photo.png as document: Retry after 19s`
- **Analysis:** sent through the `telegram_send` tool, which took the TOOL plane: telemetry shows no `kind=rich_api path=sendRichMessage` line for any of these ids (the last rich send before them is 96312), and the log carries `tool/send:` lines instead. So these answer ADR §2 rule 5 (the detached floor), NOT §3's rich plane. Two halves, and only the first is a pass: the floor CHOSE the right kind (`as document`, so §2 rule 1's size split is honoured), then Telegram answered `Retry after 19s` and the tool dropped the file without retrying. #815 closed this hole for the turn path; it is still open on the tool path. A rate-limited attachment is silently lost, and the body has already gone out so nothing can announce it.

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
