# ADR 0001 — Local file and image delivery on Telegram ("inlining")

- **Status:** Accepted (retrospective — codifies behaviour already in the tree)
- **Date:** 2026-10-29
- **Scope:** `src/utils/image.rs` (the reference walks), `src/channels/telegram/delivery.rs` (both planes),
  `src/channels/telegram/intermediates.rs` (promoted bubbles), `src/channels/telegram/file_links.rs`
  (DM deep links), `src/brain/prompt_builder.rs` (what the model is told)
- **Supersedes:** the `[channels.telegram] inline_markdown_documents` flag (deleted in `ff1425cc5`)
- **Code state:** `bdffd1878` on fork `main`. Shipped binary is `b2dcf5715` — see §9.

## 1. Context

A model writing a reply cannot upload anything. It writes a **reference to a path on the box**, and the
delivery layer decides what the reader receives:

```markdown
Here is the chart:              ![Compaction stats](/tmp/chart.png "caption")
Here is the report:             [Full write-up](/tmp/report.md)
```

Telegram gives a bot three ways to put bytes in front of a reader, and they behave very differently:

| Vehicle | What the reader gets | Can it be addressed later? |
|---|---|---|
| `sendPhoto` / `sendDocument` / `sendVideo` | its own message ("bubble"), openable | yes — a message id exists |
| A `media` entry inside `sendRichMessage` | rendered **inside** the message body | no — it is not a message |
| A `tg://<kind>?id=…` link inside a rich body | a link, **if** the media array answers it | no |

The whole history of this area is the discovery of which of those three works for which kind of file.
It was not obvious, and three separate assumptions were measured false (§7).

## 2. Decision

**Two reference families, decided by SYNTAX, not by file extension. Images inline into the rich message
body; every non-image file is delivered as its own detached bubble. Nothing else inlines.**

1. `![alt](path)` — the **image** family. Validated by magic bytes, rewritten in place to
   `![alt](tg://photo?id=imgN)`, and its bytes ride the rich message's `media` array. The picture renders
   inside the body, with the markdown title as its caption.
2. `[label](path)` — the **file** family. Never inlined, for any extension. The reference is replaced in
   place by a visible `📎 <label>` marker, and the bytes ship as their own `sendDocument` bubble *before*
   the body.
3. `<<IMG:path>>` / `<<VID:path>>` — machine markers, owned by the image / video families respectively.

The discriminator is the **`!`**. `[chart.png](/tmp/chart.png)` and `![chart.png](/tmp/chart.png)` name
the same bytes and take different roads: the first arrives as a document bubble, the second renders
inline. This is deliberate — it is the only signal the author gives about intent — and it is the single
most surprising fact in this document.

## 3. The image plane (inline)

Walk order is **video → image → file**, and it is load-bearing: a `![clip](x.mp4)` reference fails image
validation (`UnsupportedFormat`), and an image-first walk would consume it and report "Image not attached"
before the video family could claim it. So the video walk runs first, the image walk runs on its output,
and the file walk runs last on the image walk's output — one owner per reference, the same order the text
floor uses.

Per reference:

| Step | Rule |
|---|---|
| Target | local path (absolute, `~`, relative to the session cwd) → claimed. Remote URL or an existing `tg://` ref → left verbatim, never fetched, never deleted (#286's loss mode) |
| Validate | stat + 12-byte header; PNG / JPEG / GIF87a / GIF89a / WEBP / BMP only |
| Dedup | compared on the **resolved absolute** path, so `~/x.png` and `/root/x.png` dedup; an already-delivered picture is consumed and re-sent nothing |
| Rewrite | `![alt](tg://photo?id=imgN)`; a caption rides the markdown **title** (`"…"`), because the rich media array has no caption field |
| Caption gate | dropped if it contains `)`, `"` or `\`, or folds to empty — the image still arrives, merely unnamed |
| Oversize | no size gate in the rich walk. The **floor** routes ≤ 10 MiB to `sendPhoto`, above it to `sendDocument` (an un-previewable image beats a missing one) |

Reference ids are namespaced per family (`img` / `vid`) because entries are matched to references **by id
inside one message's media array** — a shared prefix would let an image entry answer a video reference.

## 4. The file plane (detached)

Every resolvable non-image file:

1. leaves the text and is replaced by `📎 <label>` (empty label → the file's own basename);
2. records **no** media entry, so the rich plane never claims it;
3. is shipped by the file floor as one `sendDocument` per file, **before** the body, with no caption (the
   caption would repeat the marker one line down);
4. the marker is then rewritten into a link to that bubble — when the chat has a link form (§6).

Rules that hold on both planes, because a link carries its own label and a silent strip would delete the
reader's only clue:

- a reference inside a code span is never touched (#464 is the open defect where a *marker* inside a
  backtick span still gets extracted);
- an unresolvable reference stays **byte-identical** and is reported to the model as a failure — never
  silently dropped (#768, #793);
- a `📎 ` the model wrote itself is consumed before the harness emits its own, so the marker is never
  doubled (#2001);
- the label is passed through `disarm_autolink`, so `harvest-report.md` does not become a fake domain
  (#1938).

Size ceiling for a document: **50 MiB** (`TELEGRAM_DOCUMENT_MAX_BYTES`), enforced *before* delivery so the
model is told "too large" instead of the channel silently refusing.

## 5. The `.md` file path

**A `.md` file is an ordinary document. It has no carve-out, no flag, and no code path of its own.**

There was one, and it is worth recording why it died, because it is the exact question this ADR exists to
settle:

| Stage | What it did | Verdict |
|---|---|---|
| `#1968` (upstream) | `.md` shipped detached by default because **Telegram Android** opens a `.md` *attachment* in its viewer but cannot open an inlined `PageBlockDocument`. Gated by `[channels.telegram] inline_markdown_documents` (default `false`) | the narrow fix, on the wrong ground |
| `#779` | a `.md` in a promoted intermediate was never delivered at all — the floor only sent files the rich plane had skipped, and a `.md` was never skipped | real bug, fixed |
| `#801` | measured: a `document` media entry is accepted with `ok:true` and **silently dropped** — in link syntax, in image syntax, with an explicit `file_name`. Only `photo` attaches. So the `.md` carve-out was not a special case of a working feature; **no document can inline at all** | the flag, its tests, its `Default` line, its prompt text and its README lines deleted in `ff1425cc5` |

Live confirmation on the shipped binary (DM `oc_l1979_bot`, 2026-10-29): `[spec.md]` → its own bubble,
`text/markdown`, 71 B; `[notes.txt]` → own bubble, `text/plain`, 35 B; `[chart.png]` → own bubble,
`image/png`, 102 119 B. Three bubbles, identical shape, zero inlining — note that the `.png` there is a
*file link*, not an image reference, so it correctly lands as a document (§2).

**There is no configuration key governing inlining today.** `inline_markdown_documents` is absent from
`TelegramConfig` (17 fields) and from all three live configs; `deny_unknown_fields` is not used, so a stale
key in a user's file is inert rather than fatal. The only knobs that touch this area are `rich_messages`
(gates the rich plane at all — off means everything is a detached bubble) and the `mermaid_*` trio.

## 6. DM vs group separation

Inlining itself is **chat-kind independent**: an image inlines in a DM exactly as in a forum topic (both
verified live). What differs is the **link form of the `📎` marker**, because a marker should lead to the
bubble that carries the file:

| Chat | `chat_id` shape | Marker form |
|---|---|---|
| Supergroup / forum topic | starts `-100…` | `https://t.me/c/<internal>/<thread>/<msg_id>` (thread in the middle) |
| Channel | starts `-100…` | `https://t.me/c/<internal>/<msg_id>` |
| **DM** | positive | `https://t.me/<bot_username>?start=file_<token>` — a **deep link**, not a message link |
| Basic group | negative, no `-100` | **plain text** — no addressable form exists |

The DM case is the subtle one. A bot never sees the client's message ids: measured in the DM, a file the
Bot API called `60451` is client message `975269`, and `messages.getHistory` over `60000..=60452` returns
nothing. `1eb7a355c` shipped `t.me/<bot>/<id>` anyway and every DM link it built was dead; it was
withdrawn in `72f21d1c4`.

A deep link sidesteps the id space entirely:

```
📎 spec.md  →  https://t.me/oc_l1979_bot?start=file_<token>
                 ↓ tap
               /start file_<token>
                 ↓
               re-send that document into the chat
```

- token: 12 hex chars of a uuid, collision-checked;
- map: `<home>/telegram_file_links.json`, a plain JSON file — disposable, so a lost map costs one re-send
  request, never a delivery;
- an unwritable map, or a file since removed, yields `None` → the plain marker, never a link that leads
  nowhere;
- the `/start` branch re-checks the allowlist, because `/start` is handled before the normal admission gate
  and a deep link must not be a way around it.

## 7. Rejected alternatives (each one measured, not argued)

| Alternative | Why it failed |
|---|---|
| Inline documents as `{"type":"document"}` media entries | API answers `ok:true` and delivers nothing — no attachment, no error, just a dead link (#801) |
| Reference a document entry in **image** syntax to force attachment | hard `400 RICH_MESSAGE_DOCUMENT_INVALID`; the same shape on a *photo* entry 400s as `RICH_MESSAGE_PHOTO_INVALID` (#800) |
| Reference a document in **link** syntax | inert for **all** media kinds — a photo entry referenced as `[x](tg://photo?…)` does not attach either. The discriminator is the markdown form, not the kind (#801 arm E) |
| A flag to keep `.md` inline | gates a behaviour that cannot work; retired with its tests (§5) |
| Address a DM bubble by `t.me/<bot>/<id>` | the bot's id space is not the client's (#802) |
| Delete a link nothing downstream will fetch | silently loses a file the author believed was delivered (#286) |
| Caption the document bubble with the marker text | the reader reads the same string twice, one line apart (#780) |

## 8. Issue census — everything filed about inlining local files and images

Fork `leshchenko1979/opencrabs` unless marked *upstream* (`opencrabs/opencrabs`).

**Images (inline plane)**

| # | State | What it settled |
|---|---|---|
| [286](https://github.com/leshchenko1979/opencrabs/issues/286) | OPEN | the origin request: extract markdown image refs as attachments; still open for non-Telegram channels |
| [289](https://github.com/leshchenko1979/opencrabs/issues/289) | CLOSED | shield unresolvable image refs so one bad URL cannot 400 the reply |
| [360](https://github.com/leshchenko1979/opencrabs/issues/360) | CLOSED | images render **inline** in the rich body, not as separate attachments |
| [368](https://github.com/leshchenko1979/opencrabs/issues/368) | OPEN | tool/cron/resume sends bypass extraction → dead markdown text |
| [464](https://github.com/leshchenko1979/opencrabs/issues/464) | OPEN | a marker inside a code span is still extracted |
| [465](https://github.com/leshchenko1979/opencrabs/issues/465) | CLOSED | the video family, and the walk order it forced |
| [487](https://github.com/leshchenko1979/opencrabs/issues/487) | OPEN | image title (caption) parsed then discarded on some paths |
| [502](https://github.com/leshchenko1979/opencrabs/issues/502) | OPEN | local image in a mid-turn intermediate stripped, never sent |
| [515](https://github.com/leshchenko1979/opencrabs/issues/515) | OPEN | the svg escape hatch ships two labels |
| [617](https://github.com/leshchenko1979/opencrabs/issues/617) | OPEN | rich-fallback cleanup deletes a bubble that carried an image |
| [731](https://github.com/leshchenko1979/opencrabs/issues/731) | OPEN | a bare `VID` token in prose swallows the rest of the reply |
| [732](https://github.com/leshchenko1979/opencrabs/issues/732) | CLOSED | a clip now inlines in the rich body |

**Files / documents (detached plane)**

| # | State | What it settled |
|---|---|---|
| [768](https://github.com/leshchenko1979/opencrabs/issues/768) | CLOSED | a `file://` target attaches instead of vanishing |
| [771](https://github.com/leshchenko1979/opencrabs/issues/771) | CLOSED | the marker links on a resume-path turn too (chat fact, not message fact) |
| [773](https://github.com/leshchenko1979/opencrabs/issues/773) | CLOSED | marker located by content, not by a stale span |
| [779](https://github.com/leshchenko1979/opencrabs/issues/779) | CLOSED | a `.md` in a promoted intermediate is delivered |
| [780](https://github.com/leshchenko1979/opencrabs/issues/780) | CLOSED | the bubble carries no caption |
| [793](https://github.com/leshchenko1979/opencrabs/issues/793) | CLOSED | a path containing a space parses |
| [800](https://github.com/leshchenko1979/opencrabs/issues/800) | CLOSED | a document entry referenced in image syntax 400s the whole send |
| [801](https://github.com/leshchenko1979/opencrabs/issues/801) | CLOSED | **the decision of this ADR**: no document inlines, ever |
| [802](https://github.com/leshchenko1979/opencrabs/issues/802) | CLOSED | a DM marker gets a deep link (§6) |

**Upstream lineage**

| # | State | What it settled |
|---|---|---|
| [#1916](https://github.com/opencrabs/opencrabs/issues/1916) | CLOSED | local file links are not delivered — resolve and send |
| [#1918](https://github.com/opencrabs/opencrabs/issues/1918) | CLOSED | the marker shape, and the `tg://document` inlining this ADR later falsified |
| [#1921](https://github.com/opencrabs/opencrabs/issues/1921) | CLOSED | the shield learned the fourth `tg://` kind |
| [#1938](https://github.com/opencrabs/opencrabs/issues/1938) | CLOSED | marker auto-linkified into a fake domain |
| [#1939](https://github.com/opencrabs/opencrabs/issues/1939) | CLOSED | rich fallback duplicated a media-bearing intermediate |
| [#1940](https://github.com/opencrabs/opencrabs/issues/1940) | CLOSED | the capabilities preamble went stale |
| [#1968](https://github.com/opencrabs/opencrabs/issues/1968) | CLOSED | the `inline_markdown_documents` flag (§5) |
| [#2001](https://github.com/opencrabs/opencrabs/issues/2001) | CLOSED | the model's own `📎` doubled the marker |

## 9. Consequences and open items

**Standing cost of the decision:** a reader cannot tap an inline document, because there is no such thing.
The floor always produces a real bubble, so the affordance is the bubble plus a link that resolves to it.

**Open, in order of what a reader would notice:**

1. **The DM deep link is not shipped.** `21d72dd9b` (plus changelog `bdffd1878`) is committed on `main`
   but `deployed.sha` is `b2dcf5715`, so a DM marker is still plain text on the running binary. Shipping
   it is a choice: tapping the marker then appends `/start file_<token>` to the chat and re-sends the file.
2. A plain-text marker in a **basic group** cannot be made tappable — no link form exists.
3. The token map grows by one entry per distinct path and is never pruned.
4. #464, #502, #617, #368 remain open on the image plane; each is a path where a reference is lost or
   mis-owned, not a disagreement with this ADR.

## 10. Verification

| Claim | Instrument |
|---|---|
| no document inlines, every extension | `every_file_the_rich_plane_cannot_take_still_has_a_delivery_leg` (`src/tests/telegram_file_delivery_test.rs`) — asserts marker emitted, `rich.entries.is_empty()`, body carries the marker, body carries no `tg://` |
| link form per chat kind | `a_supergroup_bubble_links_by_internal_id` · `a_topic_bubble_links_with_the_topic_in_the_middle` · `a_channel_links_by_the_same_rule_as_a_supergroup` · `a_dm_links_by_deep_link_and_a_basic_group_gets_none` |
| a file that cannot be linked keeps the plain marker | `a_dm_or_an_unsent_file_keeps_the_plain_marker` |
| one bubble per file, in order, no caption | `each_link_ships_its_own_bubble_in_order` · `a_resolved_link_ships_exactly_one_document_with_no_caption` |
| images inline, kind by bytes | `src/tests/telegram_local_image_media_test.rs`, `src/tests/telegram_rich_media_kind_test.rs` |
| behaviour on the shipped binary | DM probe 2026-10-29: `.md` / `.txt` / `.png` as three identical document bubbles, three `📎` markers, zero inlining |
