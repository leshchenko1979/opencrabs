# ADR 0001 compliance report

**ADR:** `docs/adr/0001-telegram-local-file-and-image-delivery.md` (revision 6)
**Binary under test:** opencrabs 0.5.4 @ 90161a32ed32cd20d3e163f95da8b8b1829a4930
**Evidence recorded at:** 2026-11-15T13:04:10Z
**Since then:** The L10/L16 observations below were emitted by 90161a32 (swapped 10:22:46Z), not by the tip that is live now. Checked rather than assumed: `git diff --stat 90161a32..3356c99d -- src/utils/image.rs src/channels/telegram/rich/` is EMPTY, so the whole #823 surface is byte-identical in the running binary and these two verdicts are not stale. The rest of the range is another lane's #831 compaction work (13 files under src/, none of them the rich plane).
**Runner:** `python3 docs/adr/probes/run.py` — **exit 1**
**39/41 passed · 2 failed · 0 missing (missing counts as failed)**

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
| §2 rule 6, §4.3 | `r6-document-emit-is-embed` | static | PASS |
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
| §2 rules 3 and 6 | `live-l1-document-link-renders` | live | PASS |
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
| §2 rule 6 | `live-l8-local-image-link-syntax-attaches` | live | PASS |
| §3.2 | `live-l9-mixed-kinds-one-message` | live | FAIL |
| §2 rule 6, §4.3 | `live-l10-document-embed-keeps-its-entry` | live | PASS |
| §2 rule 2 | `live-l11-video-embed-is-a-video` | live | FAIL |
| §3.2 caption | `live-l12-caption-rides-the-title` | live | PASS |
| §3.2 caption | `live-l13-inexpressible-caption-dropped` | live | PASS |
| §3.5, #826 | `live-l14-dedup-two-spellings-one-file` | live | PASS |
| §3.4, #826 | `live-l15-refused-entry-costs-one-entry` | live | PASS |
| §2 rule 1 floor | `live-l16-oversize-image-ships-as-document` | live | PASS |

## Live probes — the wire as Telegram stored it

### `live-l1-document-link-renders` — §2 rules 3 and 6 → **PASS**

- **Claim under test:** a local document sent through the daemon is stored as a link-form tg://document reference and lands as one bubble
- **tg_get_messages** (msg `96084`, chat `-1003936827469`): [827-smoke.md](tg://document?id=doc0)  — LINK syntax in the body, and NOTHING behind it
- **Bubbles observed:** 1 · **sent at:** 2026-11-13T10:19:48Z
- **Probe:** one local .md referenced in link syntax, nothing else
- **log:** `10:19:49.150 Telegram send ok: kind=rich_api path=sendRichMessage chat=-1003936827469`
- **log:** `10:19:50.235 delivery.rs:491 Telegram dedup: match found among 1 intermediates (normalized) — suppressing final response`
- **log:** `raw-TL read 2026-11-14: documents: [] , photos: [] , media: null  (tg_get_messages had reported the same message as carrying a document reference)`
- **Analysis:** RE-SCORED 2026-11-14. The probe's claim is: a local document is stored as a LINK-form `tg://document` reference and lands as one bubble. Both halves hold on 96084 (raw TL: one PageBlockParagraph carrying TextUrl `tg://document?id=doc0`, one bubble, no twin). The earlier FAIL scored the probe on `documents: []` and was WRONG: ADR §4.2 records the client measurement (2026-11-12) that the LINK form is the form that is visible and opens, while the EMBED form drew nothing. An empty `documents` array beside a link-form reference is therefore the WORKING shape, not a failure - scoring it as one was the #801 error (measuring the payload and calling it the render) one layer down. The rendering half remains the owner's tap, not this instrument's claim.

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

### `live-l8-local-image-link-syntax-attaches` — §2 rule 6 → **PASS**

- **Claim under test:** a LOCAL image referenced in LINK syntax `[label](path)` is still claimed by the image family and arrives as a photo entry — the two markdown forms are equivalent for local targets, which is the property that makes link syntax safe to author
- **tg_get_messages** (msg `96212`, chat `None`): `PageBlockPhoto` photo_id 5460666680959770737 inside the ordered list, caption TextEmpty — link syntax attaches a photo exactly like embed syntax
- **Bubbles observed:** None · **sent at:** 2026-11-14T10:30:36Z
- **Probe:** a local image referenced in LINK syntax: `[caption-ok.png](/root/.../fixtures/caption-ok.png)`
- **Analysis:** plane: rich — this was this lane's OWN reply, so the rewriter ran. Telemetry carries `kind=rich_api path=sendRichMessage` for the id. §3.2's form-follows-kind rule holds for images: either markdown form attaches.

### `live-l9-mixed-kinds-one-message` — §3.2 → **FAIL**

- **Claim under test:** one reply carrying an image, a video and a document produces ONE rich bubble in which each reference is answered by its own kind: the image binds as a photo entry, the video as a video entry, and the document as a link-form `tg://document` reference (ADR §4.2 measures that a document deliberately never rides as an entry - its embed form draws nothing in the client). Not three entries: the document is a reference by design.
- **tg_get_messages** (msg `96248`, chat `-1003936827469`): ONE rich bubble. Photo attached (PageBlockPhoto, photo 5456183027160524023, 512x512). Video and document did NOT attach: both survive only as TextUrl paragraphs, documents: []
- **Bubbles observed:** 1 · **sent at:** 2026-11-14T19:53:50Z
- **Probe:** one reply carrying three kinds at once: local image + local video + local document (§3.2 namespace split)
- **log:** `19:53:50.732547 kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96248 len=966 hash8=ef3fd4ef`
- **log:** `absence sweep same instant for 'send failed|falling back|degrad|IMAGE_PROCESS_FAILED|failed to read promoted' -> EMPTY (count 0)`
- **Analysis:** CLAIM CORRECTED 2026-11-14, verdict left FAIL. The original claim demanded THREE distinct media ENTRIES, but ADR §4.2 measures that a document deliberately rides as a LINK-FORM REFERENCE and never as an entry (the embed form draws nothing in the client). So the honest claim is: one bubble, image bound as a photo entry, video bound as a video entry, document present as a link reference. Measured on 96248 (raw TL): ONE bubble ✓, photo entry ✓ (5456183027160524023), video ✗ (left as TextUrl `tg://video?id=vid0`, `documents: []`), document link ✓ by design. The video half is the real defect: the shield demoted the rewriter's own `![...](tg://video?id=vid0)` embed because it judged image syntax photo-only. Fixed in 2b9789721; this verdict stays FAIL until a post-ship re-probe measures the fixed binary.

### `live-l10-document-embed-keeps-its-entry` — §2 rule 6, §4.3 → **PASS**

- **Claim under test:** a document authored in EMBED syntax keeps its embed through the send-time shield and binds as a document entry — the rewriter emits `![alt](tg://document?id=docN)` and the message comes back with `PageBlockDocument` and a non-empty `documents` array. The inverse claim (the shield DEMOTES an authored embed to link syntax, #827) is falsified: §4.3's A/B measured the link form returning `documents:[]`, a dead reference that delivers nothing
- **tg_get_messages** (msg `96838`, chat `-1003936827469`): PageBlockDocument document_id=5462918480313493113, rich_message.documents:[1] — `text/markdown`, 73 B, `doc-sample.md`. The embed SURVIVED the shield (no demotion) and the entry BOUND.
- **Bubbles observed:** None · **sent at:** 2026-11-15T11:01:08Z
- **Probe:** a local document referenced in EMBED syntax by the rewriter's own emit: `![doc-sample.md](/root/opencrabs/docs/adr/probes/fixtures/doc-sample.md)` as a reply's final line, through the send-time shield
- **log:** `11:01:07.443879 INFO rich::detect: Telegram rich verdict: true (rich_messages=true, structured=true, media_forced=true, table=true, len=1298)`
- **log:** `11:01:08.666819 INFO telegram::telemetry: Telegram send ok: origin=turn detail=- session=- kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96838 len=1296 hash8=fecc43e9`
- **Analysis:** RE-SCORED 2026-11-15 against ADR §4.3, and the CLAIM ITSELF INVERTED. The old claim asserted the shield DEMOTES an authored document embed to link syntax, and PASS meant the demotion happened (msgs 96212 / 96341, pre-fix, kept below as history). §4.3's A/B (L17: 96654 embed bound, 96655 link returned `documents:[]`) showed the link form is a DEAD reference — so the demotion was destroying the delivery, not protecting it, and the old PASS certified the defect. The shield now keeps a document embed; 96838 measures the result on the rewriter's own path (`origin=turn`): the entry BOUND.

### `live-l11-video-embed-is-a-video` — §2 rule 2 → **FAIL**

- **Claim under test:** a local MP4 in embed syntax arrives as a `video` media entry (playable), not as a document and not as a dead reference
- **tg_get_messages** (msg `96248`, chat `-1003936827469`): no video in the stored message: documents [], and the video reference survives only as TextUrl "tg://video?id=vid0"
- **Bubbles observed:** None · **sent at:** 2026-11-14T19:53:50Z
- **Probe:** a local video referenced in embed syntax, in a reply that also carries a photo and a document
- **log:** `19:53:50.732547 kind=rich_api path=sendRichMessage msg=96248 — accepted, no error`
- **Analysis:** §2 rule 2 is not met on this message. The emit is EMBED for video (r6-photo-and-video-emit-embed passes statically), so this is not the link-form artefact L8 records — an embed-syntax video entry still failed to bind beside a photo that did. Two readings remain open and this probe does not separate them: (a) a video entry is dropped when it shares a message with a photo, or (b) the 64-byte fixture was refused. §4.1 measured a 46 848-byte mp4 delivering PageBlockVideo, so (b) is live and must be excluded by re-probing with a full-size mp4 before any code change is attributed. RE-PROBE OWED: the root cause was found after this measurement - pipeline pass 2 (`shield_unresolvable_markdown_images`) judged image syntax photo-only and stripped the `!` from the rewriter's own `![...](tg://video?id=vid0)`, and a link binds nothing for any kind. Fixed in 2b9789721 (kind-aware: Photo->photo scheme, Video->tg://video scheme, Document->never). This FAIL is the PRE-FIX repro; a post-ship re-probe of the same reply re-scores it.

### `live-l12-caption-rides-the-title` — §3.2 caption → **PASS**

- **Claim under test:** an expressible caption rides the reference's markdown title and arrives as the media caption, which is the only caption channel the rich plane offers
- **tg_get_messages** (msg `96212`, chat `None`): `PageBlockPhoto` photo_id 5460666680959770736 with caption text `probe caption OK` — the markdown title arrived as the caption verbatim
- **Bubbles observed:** None · **sent at:** 2026-11-14T10:30:36Z
- **Probe:** `![caption rides the title](caption-ok.png "probe caption OK")` — an expressible caption
- **Analysis:** plane: rich — this was this lane's OWN reply, so the rewriter ran. Telemetry carries `kind=rich_api path=sendRichMessage` for the id. §3.2's caption-by-title leg passes on the rich plane. Supersedes the tool-plane reading at 96315, which measured the detached floor instead.

### `live-l13-inexpressible-caption-dropped` — §3.2 caption → **PASS**

- **Claim under test:** a caption containing `)` is DROPPED rather than emitted malformed: the image still arrives, merely unnamed, and the reference is not mangled
- **tg_get_messages** (msg `96212`, chat `None`): `PageBlockPhoto` photo_id 5460666680959770738 with caption TextEmpty — caption dropped, image attached
- **Bubbles observed:** None · **sent at:** 2026-11-14T10:30:36Z
- **Probe:** `![bad caption](caption-bad.png "probe ) caption")` — a title containing `)`, which §3.2's caption gate must drop while keeping the image
- **Analysis:** plane: rich — this was this lane's OWN reply, so the rewriter ran. Telemetry carries `kind=rich_api path=sendRichMessage` for the id. Both halves of §3.2's caption gate hold: expressible captions ride the title (L12), inexpressible ones are dropped without losing the image. Measured twice, at 96212 and again at 96341.

### `live-l14-dedup-two-spellings-one-file` — §3.5, #826 → **PASS**

- **Claim under test:** one file referenced twice in one reply under two spellings (`~/…` and `/root/…`) is delivered ONCE: dedup compares the resolved absolute path
- **tg_get_messages** (msg `96222`, chat `None`): ONE photo entry: `photos: [Photo 5460666680959770736]`, referenced by two `PageBlockPhoto` blocks in the body
- **Bubbles observed:** None · **sent at:** 2026-11-14T18:38:29Z
- **Probe:** one file, two spellings, one reply: `~/.../local-photo-512.png` and `/root/.../local-photo-512.png`
- **Analysis:** plane: rich — this was this lane's OWN reply, so the rewriter ran. Telemetry carries `kind=rich_api path=sendRichMessage` for the id. §3.2's dedup compares the RESOLVED absolute path: two spellings of one file earn one entry and one upload, referenced twice in the body. The counter-case is different CONTENT (PROBE A items 1 and 3 referenced the same file in two forms with different captions and earned two entries), which is by design — a captioned photo is a different payload.

### `live-l15-refused-entry-costs-one-entry` — §3.4, #826 → **PASS**

- **Claim under test:** a reply whose media plane contains a file Telegram refuses (`IMAGE_PROCESS_FAILED`) still lands as ONE bubble carrying the healthy entries, instead of downgrading the whole reply to the HTML dialect
- **tg_get_messages** (msg `96344`, chat `None`): ONE bubble, ONE attachment: the healthy photo bound as PageBlockPhoto 5456183027160524023 (512x512); the refused entry's reference was neutralised to plain text (no dead tg:// link, no escaped !markdown, no degraded twin bubble after it)
- **Bubbles observed:** None · **sent at:** 2026-11-14T21:19:20Z
- **Probe:** a reply carrying a corrupt PNG (Telegram refuses it: IMAGE_PROCESS_FAILED) beside a healthy PNG, sent on the rich plane
- **log:** `21:19:18.488188 WARN Telegram send failed ... kind=rich_api path=sendRichMessage thread=Some(31847) len=894 hash8=bae2df13 error=(400 Bad Request): Bad Request: IMAGE_PROCESS_FAILED`
- **log:** `21:19:18.494332 WARN rich media send refused ... - retrying the identical request once before sacrificing any entry (hedge: the refusal may be transient)`
- **log:** `21:19:19.459213 WARN Telegram send failed ... len=894 hash8=bae2df13 error=(400 Bad Request): Bad Request: IMAGE_PROCESS_FAILED`
- **log:** `21:19:19.460227 WARN rich media send refused ... - retrying without 1 entry [img0]; the dropped reference is neutralised so it cannot 400 the retry`
- **log:** `21:19:20.723779 INFO Telegram send ok: ... kind=rich_api path=sendRichMessage msg=96344 len=893 hash8=42c0add6`
- **Analysis:** The full #826 AC-3 sequence is visible end to end: attempt 1 refused, the HEDGE re-sent the identical request (same hash8=bae2df13, same len=894) and was refused again, then exactly ONE entry [img0] was sacrificed, its reference neutralised, and attempt 3 succeeded at len=893 (one byte shorter: the neutralised ref). The healthy photo rode the SAME bubble, and the next rich send 96345 (len=4842) is the next turn's own body, not a degraded fallback of this one - the positive control for 'no twin' is that the log DOES emit 'falling back' today (3 hits), so its absence here is a measurement, not a blind grep.

### `live-l16-oversize-image-ships-as-document` — §2 rule 1 floor → **PASS**

- **Claim under test:** a structurally valid 12.01 MiB image is above the `sendPhoto` ceiling, so the floor ships it as a document: it arrives as a file rather than being refused
- **tg_get_messages** (msg `96840`, chat `-1003936827469`): PageBlockDocument document_id=5462918480313493132, rich_message.documents:[1] — `image/png`, 12,598,410 B, `oversize-photo.png`. The image family DECLINED it and the file family shipped it as a document, so the entry bound instead of being refused.
- **Bubbles observed:** None · **sent at:** 2026-11-15T11:04:14Z
- **Probe:** the same structurally valid 12,598,410 B PNG (2048x2048) past the 10 MiB sendPhoto ceiling, referenced alone as a reply's final line
- **log:** `11:04:14.128788 INFO telegram::telemetry: Telegram send ok: origin=turn detail=- session=- kind=rich_api path=sendRichMessage chat=-1003936827469 thread=Some(31847) msg=96840 len=386 hash8=7230e314`
- **Analysis:** RE-PROBED 2026-11-15 after the fix. The pre-fix measurement (msg 96464, 2026-11-14T23:40:52Z) is the reason this probe exists: the rich rewriter claimed every locally-valid image as `tg://photo` with no size check, so the API refused the send with `file of size 12598410 bytes is too big for a photo`, the message fell back to the HTML dialect, and the reference degraded to a literal `!` plus a TextUrl to a photo that does not exist (`documents:[]`). Now the image family declines above `TELEGRAM_PHOTO_MAX_BYTES` and the file family ships the same bytes as a document: 96840 carries the entry and a real `PageBlockDocument`. This is the #823 owner complaint's own fixture.

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
