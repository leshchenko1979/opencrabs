//! Mermaid diagram rendering for Telegram rich messages (#1044).
//!
//! Model output frequently carries ```mermaid fences. They are embedded via
//! `sendRichMessage`: the PRIMARY path is the markdown input mode plus the
//! Bot API 10.2 `media` field (`tg://photo?id=` references), which keeps any
//! tables in the message native; the HTML input mode (`<img>`) is the
//! fallback for servers without the `media` field. A broken image URL makes
//! the whole send fail with `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND`, so before
//! delivery each fence is pre-validated against the renderer (mermaid.ink):
//! the render's PNG bytes are fetched by us and uploaded via multipart
//! (`attach://`), so Telegram never fetches a third-party URL. A request
//! ladder keeps the render inside Telegram's photo box (natural size first,
//! then a proportional 1200px width clamp); anything else degrades to a
//! legible failure block (the renderer's error note plus the original
//! source) instead of killing the message. Pre-validation never panics or
//! hangs; failure paths yield [`MermaidResult::Failed`].

use super::ast::Block;
pub use super::ast::MermaidResult;
use crate::channels::telegram::markdown::escape_html;
use crate::tui::render::theme;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::json;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

// ── Channel-agnostic Mermaid vocabulary (#326) ──
//
// The pure fence parser lives in `crate::utils::mermaid` so core can validate
// model output without depending on this channel. Re-exported here so the
// channel's own callers and the test files importing from `rich::mermaid` keep
// one import path per name (house style, see `rich/mod.rs`).
pub(crate) use crate::utils::mermaid::{
    find_mermaid_fences, has_mermaid_fence, looks_like_mermaid_source,
};

/// Base URL of the mermaid.ink image renderer. The diagram source is
/// base64url-appended. NOTE: this sends the diagram text to a third party.
const MERMAID_INK_BASE: &str = "https://mermaid.ink/img/";
/// Vector endpoint base for [`ink_url_svg`].
pub(crate) const MERMAID_INK_SVG_BASE: &str = "https://mermaid.ink/svg/";

/// Ladder rung 2's width clamp: `width` scales the render proportionally
/// (measured: the stress case goes 1611×3727 → 1200×2776), so a single clamp
/// rung covers any realistic aspect ratio without a local redraw.
const MERMAID_INK_CLAMP_WIDTH: &str = "width=1200";

/// The `surface_code` colour `auto` falls back to when the active theme has
/// no RGB form (an index or named colour the terminal resolves itself).
/// Matches `crate::tui::render::palette::SURFACE_CODE`, the `crab-dark`
/// default.
const AUTO_SURFACE_HEX: &str = "#282d37";

/// Relative luminance below which a surface counts as dark (#318). The
/// midpoint of WCAG's 0..=1 range: a surface lighter than mid-grey takes the
/// light glyph palette, a darker one the dark palette.
const DARK_SURFACE_LUMINANCE: f64 = 0.5;

/// The mermaid theme names mermaid.ink accepts. Anything else is a config
/// typo, and falls back to the luminance-derived default with a warning.
const MERMAID_THEME_NAMES: [&str; 5] = ["default", "dark", "neutral", "forest", "base"];

/// The `themeVariables` mermaid.ink actually honours, each proven by a
/// single-key wire probe against a fixed baseline (base md5 `5780049e88a8`).
///
/// Five further keys were probed and moved ZERO pixels — `primaryColor`,
/// `primaryBorderColor`, `secondaryColor`, `background`,
/// `edgeLabelBackground` — and are deliberately absent: emitting them would
/// enlarge every payload for no effect.
#[derive(Debug, Clone)]
pub(crate) struct Palette {
    pub(crate) line: String,
    pub(crate) node_border: String,
    pub(crate) main_bkg: String,
    pub(crate) cluster_bkg: String,
    pub(crate) cluster_border: String,
    pub(crate) text: String,
}

/// How one mermaid render is styled (#318): the glyph palette, the opaque
/// background, and the connector and fill colours that palette carries.
///
/// Before #318 the renderer was asked for a bare `?type=png`, so mermaid.ink
/// returned an RGBA PNG whose background is fully transparent — and a dark
/// client composited mermaid's LIGHT palette onto WHITE (owner report
/// 2026-09-18: "My tg is dark, but the mermaids are white"). `bgColor` is the
/// parameter that removes the transparency; `theme` only selects the glyph
/// palette, and `theme=dark` ALONE makes things worse (light glyphs on the
/// still-transparent white). Both must travel together.
///
/// mermaid.ink accepts the diagram either as base64url of the raw source or as
/// base64url of a JSON **state object**, and only the JSON form carries
/// `themeVariables` — the plain-code form ignores it silently (measured:
/// `lineColor: #ff0000` moved 0 px as code, 180 px as JSON). So [`payload`]
/// always emits the JSON form, and `theme` and `themeVariables` ride inside it
/// while `bgColor` and `width` stay query parameters ([`query`]).
///
/// [`payload`]: MermaidStyle::payload
/// [`query`]: MermaidStyle::query
#[derive(Debug, Clone)]
pub(crate) struct MermaidStyle {
    /// The mermaid glyph palette name.
    pub(crate) theme: &'static str,
    /// Opaque background as BARE 6-hex (no `#`), or `None` for the pre-#318
    /// transparent look. Bare is not cosmetic: mermaid.ink SILENTLY ignores a
    /// `#`-prefixed, percent-encoded, named or wrong-length `bgColor` and
    /// hands back a transparent PNG — the original defect, invisibly.
    /// [`resolve_bg`] is the only writer.
    pub(crate) bg: Option<String>,
    pub(crate) palette: Palette,
}

impl MermaidStyle {
    /// Resolve the style from `[channels.telegram]` plus the ACTIVE TUI
    /// theme. Both keys default to `auto`, which means "follow the theme the
    /// app is already drawing with" — the Telegram Bot API exposes no client
    /// dark/light preference, so the server's own surface is the only honest
    /// reference available.
    pub(crate) fn from_config() -> Self {
        let tg = &crate::config::Config::current().channels.telegram;
        Self::from_values(&tg.mermaid_theme, &tg.mermaid_bg)
    }

    /// [`from_config`] against explicit values, so the normalisation rules are
    /// unit-testable without touching the process-global config.
    ///
    /// [`from_config`]: MermaidStyle::from_config
    pub(crate) fn from_values(theme_cfg: &str, bg_cfg: &str) -> Self {
        let surface_color = theme::active().colors.surface_code;
        let (surface_hex, dark) = match theme::hex_of(surface_color) {
            Some(hex) => (
                hex,
                theme::relative_luminance(surface_color) < DARK_SURFACE_LUMINANCE,
            ),
            // No portable hex: the terminal resolves this colour against its
            // own palette, so fall back to the crab-dark surface as a dark
            // one rather than guessing.
            None => (AUTO_SURFACE_HEX.to_string(), true),
        };
        Self {
            theme: resolve_theme_name(theme_cfg, dark),
            bg: resolve_bg(bg_cfg, &surface_hex),
            palette: Palette::for_surface(&surface_hex, dark),
        }
    }

    /// The query string for one request: `?type=png`, then the opaque
    /// background when one is set, then `extra`.
    ///
    /// `extra` is empty for the natural-size rung — no width or scale
    /// override, so mermaid.ink returns the diagram at its intrinsic
    /// Chromium/ELK render size (the 44-node stress case: 1611×3727; the old
    /// `width=1600&scale=2` override doubled that to 3200×7404, past
    /// Telegram's photo box) — and the clamp width for the clamp rung. The
    /// SVG link builds through the same call, so the vector and the embedded
    /// PNG can never disagree about the render.
    pub(crate) fn query(&self, extra: &str) -> String {
        let mut q = String::from("?type=png");
        if let Some(bg) = &self.bg {
            q.push_str("&bgColor=");
            q.push_str(bg);
        }
        if !extra.is_empty() {
            q.push('&');
            q.push_str(extra);
        }
        q
    }

    /// The base64url payload for one diagram: mermaid.ink's JSON state object,
    /// carrying the source and the `mermaid` config (theme + palette).
    ///
    /// `themeVariables` MUST sit INSIDE the `mermaid` object. mermaid.ink
    /// reads the top-level `mermaid` key as the mermaid CONFIG and ignores
    /// every other top-level key but `code`/`autoSync`/`updateDiagram`, so a
    /// top-level `themeVariables` is silently discarded. Measured on the wire
    /// 2026-09-19, same diagram, same `?type=png&bgColor=282d37`: hoisting the
    /// palette to the top level renders BYTE-IDENTICAL to a payload carrying
    /// no palette at all (19802 B, zero `#ffffff` pixels, connector contrast
    /// 9.22:1); the nested form lands it (20318 B, 95 `#ffffff` pixels,
    /// connector contrast 13.81:1). Nested is the only form that reaches the
    /// renderer.
    pub(crate) fn payload(&self, source: &str) -> String {
        let p = &self.palette;
        base64url(
            &json!({
                "code": source,
                "mermaid": {
                    "theme": self.theme,
                    "themeVariables": {
                        "lineColor": p.line,
                        "nodeBorder": p.node_border,
                        "mainBkg": p.main_bkg,
                        "clusterBkg": p.cluster_bkg,
                        "clusterBorder": p.cluster_border,
                        "textColor": p.text,
                    },
                },
            })
            .to_string(),
        )
    }

    /// The render-cache key: every field that changes the rendered bytes, plus
    /// the source. Without the style in the key a render cached under one
    /// theme could be served for another. The #100 trailing-newline
    /// normalisation is folded in here so both cache legs share it by
    /// construction.
    pub(crate) fn cache_key(&self, source: &str) -> String {
        let p = &self.palette;
        format!(
            "{theme}|{bg}|{line}|{nb}|{mb}|{cb}|{clb}|{txt}|{src}",
            theme = self.theme,
            bg = self.bg.as_deref().unwrap_or("none"),
            line = p.line,
            nb = p.node_border,
            mb = p.main_bkg,
            cb = p.cluster_bkg,
            clb = p.cluster_border,
            txt = p.text,
            src = source.trim_end_matches('\n'),
        )
    }
}

impl Palette {
    /// Derive the honoured palette from a surface colour (#318).
    ///
    /// The offsets are what make this track ANY theme rather than being
    /// hardcoded to `crab-dark`: node and subgraph fills sit a fixed step
    /// above (dark surface) or below (light surface) the surface itself, so
    /// shapes read as cards on the diagram's own background instead of as
    /// holes. For `crab-dark` (`#282d37`) that yields the measured worked
    /// example — `mainBkg #333a45`, `clusterBkg #2f3542` — at 1.20:1 and
    /// 1.12:1 against the surface, with `lineColor #ffffff` lifting
    /// connector-only rows from 7.66:1 to 11.28:1 (stroke 13.81:1).
    fn for_surface(surface_hex: &str, dark: bool) -> Self {
        let base = parse_rgb(surface_hex).unwrap_or((0x28, 0x2d, 0x37));
        let (lift, tint) = if dark {
            ((11, 13, 14), (7, 8, 11))
        } else {
            ((-11, -13, -14), (-7, -8, -11))
        };
        Self {
            line: if dark { "#ffffff" } else { "#333333" }.to_string(),
            node_border: if dark { "#ffffff" } else { "#9aa0a6" }.to_string(),
            main_bkg: shift(base, lift),
            cluster_bkg: shift(base, tint),
            cluster_border: "#6b7280".to_string(),
            text: if dark { "#e6e6e6" } else { "#1a1a1a" }.to_string(),
        }
    }
}

/// Resolve the glyph palette name: `auto` (or blank) follows the surface's
/// luminance, a known mermaid theme name passes through, and anything else is
/// a config typo that falls back to the `auto` result. The warning is not
/// decoration — a typo'd value would otherwise change the render with no trace
/// anywhere.
fn resolve_theme_name(cfg: &str, dark: bool) -> &'static str {
    let auto = if dark { "dark" } else { "default" };
    let name = cfg.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("auto") {
        return auto;
    }
    match MERMAID_THEME_NAMES
        .iter()
        .find(|known| name.eq_ignore_ascii_case(known))
    {
        Some(known) => known,
        None => {
            tracing::warn!(
                value = %cfg,
                fallback = auto,
                "mermaid_theme not recognized; using the auto palette"
            );
            auto
        }
    }
}

/// Resolve the background: `auto` (or blank) uses the active theme's surface,
/// `none` restores the pre-#318 transparency, and a 6-hex value is taken
/// verbatim (normalised to bare lowercase). Anything else falls back to the
/// `auto` result — silently accepting it would ship a transparent PNG, which
/// is the defect this whole change exists to remove.
fn resolve_bg(cfg: &str, surface_hex: &str) -> Option<String> {
    let value = cfg.trim();
    if value.eq_ignore_ascii_case("none") {
        return None;
    }
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        return Some(bare_hex(surface_hex));
    }
    match normalize_hex(value) {
        Some(hex) => Some(hex),
        None => {
            tracing::warn!(
                value = %cfg,
                fallback = %bare_hex(surface_hex),
                "mermaid_bg not recognized; using the active surface"
            );
            Some(bare_hex(surface_hex))
        }
    }
}

/// Accept `#RRGGBB` or `RRGGBB` and return the bare lowercase 6-hex mermaid.ink
/// requires. Reuses the user-theme hex parser so the two agree on what a colour
/// is; the `#` is dropped because a `#`-prefixed `bgColor` is silently ignored
/// on the wire.
fn normalize_hex(value: &str) -> Option<String> {
    crate::tui::render::user_themes::parse_hex(value).map(|_| bare_hex(value))
}

/// Strip a leading `#` and lowercase — the form `bgColor` accepts on the wire.
fn bare_hex(value: &str) -> String {
    value.trim().trim_start_matches('#').to_ascii_lowercase()
}

/// Parse a bare or `#`-prefixed 6-hex colour into its channels.
fn parse_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Shift each channel of `base` by `delta`, clamped to the byte range, and
/// render the result as `#rrggbb`.
fn shift(base: (u8, u8, u8), delta: (i16, i16, i16)) -> String {
    let ch = |v: u8, d: i16| -> u8 { (i16::from(v) + d).clamp(0, 255) as u8 };
    format!(
        "#{:02x}{:02x}{:02x}",
        ch(base.0, delta.0),
        ch(base.1, delta.1),
        ch(base.2, delta.2)
    )
}

/// Telegram rejects photos whose width+height exceeds this combined budget
/// (measured live, #1238: 3200×7404 refused, 1611×3727 accepted).
const PHOTO_MAX_TOTAL_DIMS: f32 = 9_600.0;

/// Total budget for one renderer fetch — connect, headers AND body (#189).
/// The body is the expensive leg: live renders measured 12 KB–140 KB
/// (2026-09-12), so this must cover a slow stream rather than only a slow
/// handshake. A stalled download degrades to a legible failure block.
pub(crate) const PREVALIDATE_TIMEOUT_SECS: u64 = 30;

/// Connect-phase budget, split out of the total (#189). A renderer that will
/// not accept a connection within this window is down, and spending the whole
/// body budget waiting for a handshake wastes the ladder. Before the split a
/// single 10s total let a slow body masquerade as an unreachable host.
pub(crate) const PREVALIDATE_CONNECT_TIMEOUT_SECS: u64 = 5;

/// One bounded re-fetch after a transient body failure (#189). By then the
/// response has already passed the `2xx + image/*` check, so the render
/// exists server-side and the retry is a cheap re-read of bytes we know are
/// there — one network wobble should not cost the picture.
const BODY_RETRY_DELAY_MS: u64 = 300;

/// Cap on how much of the renderer's error body we surface, so a huge HTML
/// error page can't blow up the message.
const ERROR_NOTE_MAX_CHARS: usize = 400;

/// One media reference embedded via the markdown `media` field (#1044).
/// `id` matches the `tg://photo?id=<id>` reference in the markdown text.
///
/// Exactly one of the payload sources is set:
/// - `url`: the mermaid.ink renderer image URL Telegram fetches server-side
///   (the legacy, network-dependent path).
/// - `bytes`: the pre-validated mermaid.ink PNG bytes, uploaded to Telegram
///   via multipart as `attach://<id>` — the active delivery mode; Telegram
///   never touches a third-party URL.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MediaEntry {
    pub(crate) id: String,
    pub(crate) url: Option<String>,
    pub(crate) bytes: Option<Vec<u8>>,
}

/// Encode `input` as base64url (RFC 4648 §5, no padding), the alphabet
/// mermaid.ink requires. Standard base64 (`+`, `/`) returns 404 there.
pub(crate) fn base64url(input: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input.as_bytes())
}

/// Whether `text` should be routed through the mermaid render path:
/// rich messages are enabled, the `mermaid_render` flag is on, and the text
/// actually contains a mermaid fence. Requires `rich_messages` because the
/// image can only be embedded via `sendRichMessage`.
pub(crate) fn should_render_mermaid(text: &str) -> bool {
    let tg = &crate::config::Config::current().channels.telegram;
    tg.rich_messages && tg.mermaid_render && has_mermaid_fence(text)
}

/// Full mermaid.ink embed URL for a diagram source: the style's b64url
/// payload plus the natural-size query ([`MermaidStyle::query`]).
pub(crate) fn ink_url(style: &MermaidStyle, source: &str) -> String {
    ink_url_params(style, source, "")
}

/// The dedicated vector endpoint for a diagram source (owner directive
/// 2026-09-10 03:56Z: "for the plain html - instead of an error message, we
/// should give a mermaid svg link"). Same styled b64url payload and the same
/// query as [`ink_url`], over `https://mermaid.ink/svg/` — the endpoint
/// serves a real `image/svg+xml` (live-verified 2026-09-10); `?type=svg` on
/// `/img/` does NOT. Build-only URL: the browser does the fetch, the server
/// never touches mermaid.ink for this.
pub(crate) fn ink_url_svg(style: &MermaidStyle, source: &str) -> String {
    format!(
        "{}{}{}",
        MERMAID_INK_SVG_BASE,
        style.payload(source),
        style.query("")
    )
}

/// The same URL at an explicit rung — `extra` empty for natural size, the
/// clamp width for the ladder's second rung.
fn ink_url_params(style: &MermaidStyle, source: &str, extra: &str) -> String {
    format!(
        "{}{}{}",
        MERMAID_INK_BASE,
        style.payload(source),
        style.query(extra)
    )
}

/// Standard mobile viewport width and single-screen reading box dimensions.
/// Diagrams exceeding these dimensions or hitting width clamp rungs are considered capped/scaled (>= 1x).
pub(crate) const STANDARD_VIEWPORT_MAX_WIDTH: u32 = 1200;
pub(crate) const STANDARD_VIEWPORT_MAX_HEIGHT: u32 = 1800;

/// Whether a diagram's dimensions represent a capped or scaled-down render (>= 1x).
/// When width exceeds standard reading width (1200px), height exceeds comfortable
/// vertical reading bounds (1800px), or aspect ratio is extreme, Telegram mobile
/// scales the image down substantially, rendering small text illegible.
pub(crate) fn is_diagram_capped(w: u32, h: u32) -> bool {
    w > STANDARD_VIEWPORT_MAX_WIDTH
        || h > STANDARD_VIEWPORT_MAX_HEIGHT
        || (w > 0 && h > 0 && ((w as f32 / h as f32) > 2.5 || (h as f32 / w as f32) > 3.0))
}

/// Whether a render fits Telegram's photo box (width + height budget).
/// Pure f32 arithmetic with no renderer deps — kept out of the
/// feature-gated local-render module so every build can dimension-check
/// remote PNGs.
pub(crate) fn photo_fits(w: u32, h: u32) -> bool {
    (w as f32) + (h as f32) <= PHOTO_MAX_TOTAL_DIMS
}

/// Parse a PNG's IHDR header for its (width, height). Returns `None` for
/// non-PNG bodies and buffers too short to carry the header. Split out so
/// the oversize ladder is unit-testable without a network call.
pub(crate) fn png_dims(png: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if png.len() < 24 || png[..8] != PNG_SIG {
        return None;
    }
    if png[12..16] != *b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Some((w, h))
}

// ── Render cache (#37) ──
//
// The regen-nudge preflight resolves the same fence the delivery path
// resolves moments later, and mermaid.ink renders are deterministic:
// identical source, identical outcome. Caching the fresh outcome spares the
// renderer the duplicate round-trip — and spares the delivery path a second
// chance to hit a transient wobble on an already-proven source. Only
// deterministic outcomes are stored: rendered image bytes and parse errors.
// Transient `Failed` outcomes are never pinned, because the same source may
// render fine seconds later.

/// Freshness window for cached render outcomes (#37).
const RENDER_CACHE_TTL_SECS: u64 = 600;

/// Most cached render outcomes held; the oldest entry is evicted when the
/// cap is reached (#37).
const RENDER_CACHE_CAP: usize = 64;

/// Cached render outcomes keyed by diagram source: insertion time plus the
/// outcome itself (#37).
static RENDER_CACHE: LazyLock<Mutex<HashMap<String, (Instant, MermaidResult)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A fresh cached render outcome for `style` and `source`, if any (#37).
/// Expired entries are swept on every lookup so an idle cache cannot bloat.
///
/// The key comes from [`MermaidStyle::cache_key`], which folds in the style as
/// well as the #100 normalisation — trailing newlines are stripped because the
/// raw-markdown fence finder keeps the fence body's trailing newline while the
/// parser's `Block::Code.text` drops it, so without this the same diagram got
/// two keys one byte apart and each delivery surface re-paid the render.
pub(crate) fn cache_get(style: &MermaidStyle, source: &str) -> Option<MermaidResult> {
    let key = style.cache_key(source);
    let mut cache = RENDER_CACHE.lock().ok()?;
    let now = Instant::now();
    cache.retain(|_, (at, _)| now.duration_since(*at) < Duration::from_secs(RENDER_CACHE_TTL_SECS));
    cache.get(&key).map(|(_, outcome)| outcome.clone())
}

/// Store a deterministic render outcome for `style` and `source` (#37). Only
/// [`MermaidResult::ImageBytes`] and [`MermaidResult::ParseError`] are
/// stored; transient failures are skipped so a retryable outage is never
/// pinned as a stuck failure block. Evicts the oldest entry at the cap.
pub(crate) fn cache_put(style: &MermaidStyle, source: &str, outcome: &MermaidResult) {
    let key = style.cache_key(source); // #100: shared normalized key
    if !matches!(
        outcome,
        MermaidResult::ImageBytes(_) | MermaidResult::ParseError(_)
    ) {
        return;
    }
    let Ok(mut cache) = RENDER_CACHE.lock() else {
        return;
    };
    if cache.len() >= RENDER_CACHE_CAP && !cache.contains_key(&key) {
        let oldest = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(k, _)| k.clone());
        if let Some(oldest_key) = oldest {
            cache.remove(&oldest_key);
        }
    }
    cache.insert(key, (Instant::now(), outcome.clone()));
}

/// Hand the render outcome to the caller, caching it first when it is
/// deterministic (#37).
fn finish(style: &MermaidStyle, source: &str, outcome: MermaidResult) -> MermaidResult {
    cache_put(style, source, &outcome);
    outcome
}

/// Record a render failure and return it (#189). Before this helper every
/// non-image exit returned [`MermaidResult::Failed`] with ZERO log output, so
/// a dropped body left no trace in the daemon log and incidents were argued
/// from absence — the same defect class #64 fixed for non-200 responses, where
/// the body leg was simply missed. `stage` names the ladder rung, and the
/// `timed_out` split is what distinguishes a stalled download from a refused
/// connection; both used to print the identical user-facing string.
fn fail(
    stage: &str,
    source: &str,
    note: impl Into<String>,
    err: Option<&reqwest::Error>,
) -> MermaidResult {
    let note = note.into();
    let detail = err.map(|e| e.to_string()).unwrap_or_default();
    tracing::warn!(
        stage,
        source_len = source.len(),
        timed_out = err.map(|e| e.is_timeout()).unwrap_or(false),
        error = %detail,
        note = %note,
        "mermaid render failed"
    );
    MermaidResult::Failed(note)
}

/// Read a render body in full, retrying ONCE on a transient failure (#189).
/// The caller only reaches this with a response that already passed the
/// `2xx + image/*` check, so the render exists server-side: one mid-stream
/// drop is worth exactly one cheap re-fetch before we degrade to a failure
/// block. `stage` labels the rung (`body` / `body-clamp`) so the retry rung is
/// distinguishable in the log while the user-facing note stays clean.
async fn read_body_with_retry(
    client: &reqwest::Client,
    url: &str,
    first: reqwest::Response,
    source: &str,
    stage: &str,
) -> Result<Vec<u8>, MermaidResult> {
    match first.bytes().await {
        Ok(b) => return Ok(b.to_vec()),
        Err(e) => {
            tracing::warn!(
                stage,
                source_len = source.len(),
                timed_out = e.is_timeout(),
                error = %e,
                "mermaid render body read failed; retrying once"
            );
        }
    }
    tokio::time::sleep(Duration::from_millis(BODY_RETRY_DELAY_MS)).await;
    let retry = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            return Err(fail(
                &format!("{stage}-retry"),
                source,
                "diagram renderer unreachable",
                Some(&e),
            ));
        }
    };
    let status = retry.status().as_u16();
    let content_type = retry
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !is_image_response(status, &content_type) {
        let body = retry.text().await.unwrap_or_default();
        return Err(fail(
            &format!("{stage}-retry"),
            source,
            error_note(status, &body),
            None,
        ));
    }
    match retry.bytes().await {
        Ok(b) => Ok(b.to_vec()),
        Err(e) => Err(fail(
            &format!("{stage}-retry"),
            source,
            "diagram renderer dropped the image",
            Some(&e),
        )),
    }
}

/// Pre-validate a single mermaid diagram against the renderer. On HTTP 200
/// with an `image/*` content type it DOWNLOADS the rendered PNG and returns
/// [`MermaidResult::ImageBytes`] — Telegram never fetches a URL from us
/// (its own URL fetcher proved the unreliable link: `400 failed to get
/// HTTP URL content` on live probes while the same URL fetched fine from
/// this host). Every other outcome (non-200, non-image, timeout, transport
/// error, client build failure, dropped body) yields
/// [`MermaidResult::Failed`] with a legible note. Never panics, never hangs
/// past the timeout.
///
/// Delivery is remote-only (#1238, owner directive: the in-process
/// renderer's visual quality is not acceptable in production, and the owner
/// later ordered it removed from the tree entirely): the request ladder
/// walks natural size → proportional width clamp, both served by
/// mermaid.ink; if every rung fails or still busts Telegram's photo box,
/// the fence degrades to a legible failure block. No local renderer exists.
pub(crate) async fn resolve(style: &MermaidStyle, source: &str) -> MermaidResult {
    // #37: reuse a fresh render — the regen preflight and the delivery path
    // resolve the same source under the same style, and the renderer's
    // outcome is deterministic.
    if let Some(cached) = cache_get(style, source) {
        return cached;
    }

    let url = ink_url(style, source);

    let client = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(
            PREVALIDATE_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(std::time::Duration::from_secs(PREVALIDATE_TIMEOUT_SECS))
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail("client", source, "diagram renderer unavailable", Some(&e)),
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            let note = if e.is_timeout() {
                "diagram renderer timed out"
            } else {
                "diagram renderer unreachable"
            };
            return fail("connect", source, note, Some(&e));
        }
    };

    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if is_image_response(status, &content_type) {
        // Bytes delivery (hybrid round 2): download the PNG here and hand
        // Telegram the bytes via multipart (`attach://`). Telegram's own
        // server-side URL fetcher is the unreliable hop — it 400s with
        // "failed to get HTTP URL content" on URLs this host fetches fine —
        // so Telegram never sees a URL at all.
        let body = match read_body_with_retry(&client, &url, resp, source, "body").await {
            Ok(b) => b,
            Err(outcome) => return outcome,
        };
        // Dimension ladder (#1238): natural size first; if it busts
        // Telegram's photo box, re-request the SAME diagram with a
        // proportional 1200px width clamp — still mermaid.ink, still
        // Chromium/ELK quality. Delivery never redraws locally.
        match png_dims(&body) {
            Some((w, h)) if !photo_fits(w, h) => {
                tracing::warn!(
                    w,
                    h,
                    "natural render exceeds the photo box; retrying at the width clamp"
                );
                let clamp_url = ink_url_params(style, source, MERMAID_INK_CLAMP_WIDTH);
                let cresp = match client.get(&clamp_url).send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return fail(
                            "clamp-connect",
                            source,
                            format!(
                                "rendered diagram {w}x{h} exceeds the photo box and the width-clamp retry failed"
                            ),
                            Some(&e),
                        );
                    }
                };
                let cstatus = cresp.status().as_u16();
                let ctype = cresp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                if !is_image_response(cstatus, &ctype) {
                    let cbody = cresp.text().await.unwrap_or_default();
                    return fail("clamp-response", source, error_note(cstatus, &cbody), None);
                }
                let cbytes =
                    match read_body_with_retry(&client, &clamp_url, cresp, source, "body-clamp")
                        .await
                    {
                        Ok(b) => b,
                        Err(outcome) => return outcome,
                    };
                if let Some((cw, ch)) = png_dims(&cbytes).filter(|&(cw, ch)| !photo_fits(cw, ch)) {
                    return fail(
                        "clamp-dims",
                        source,
                        format!(
                            "rendered diagram exceeds the photo box even at the width clamp: {cw}x{ch} px"
                        ),
                        None,
                    );
                }
                tracing::info!(
                    bytes = cbytes.len(),
                    "mermaid.ink clamp render ok; delivering bytes"
                );
                return finish(style, source, MermaidResult::ImageBytes(cbytes.to_vec()));
            }
            _ => {}
        }
        tracing::info!(
            bytes = body.len(),
            "mermaid.ink render ok; delivering bytes"
        );
        return finish(style, source, MermaidResult::ImageBytes(body.to_vec()));
    }

    // Not a usable image: surface the renderer's own error text (mermaid.ink
    // returns a plain-text parse error) so the failure block is legible.
    let body = resp.text().await.unwrap_or_default();
    // Failure telemetry (leshchenko1979/opencrabs#64): the success leg logs
    // `render ok`; without a matching warn here a non-200 is invisible in
    // the daemon log and incidents get argued from absence.
    tracing::warn!(
        status,
        note = %error_note(status, &body),
        "mermaid.ink render failed"
    );
    finish(style, source, classify_render_failure(status, &body))
}

/// Pre-render every mermaid fence in `text` and collect the PARSE errors
/// (#37). Called from the tool loop before the reply is final: each fence
/// is resolved through [`resolve`] — which populates the render cache, so
/// the delivery path reuses every outcome instead of re-asking the
/// renderer — and only deterministic parse rejections are reported.
/// Transient failures stay silent here; the delivery path degrades them
/// to legible failure blocks as before. An empty `Vec` means every fence
/// rendered.
pub(crate) async fn preflight_parse_errors(style: &MermaidStyle, text: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for fence in find_mermaid_fences(text) {
        if let MermaidResult::ParseError(note) = resolve(style, &fence.source).await {
            errors.push(note);
        }
    }
    errors
}

/// Classify a non-image renderer response (#37): HTTP 4xx — except the
/// transient 408/429 — is a deterministic PARSE rejection of this exact
/// source; mermaid.ink answers it with plain-text error text naming the
/// offending construct, which the model can act on (regen nudge). Anything
/// else (server errors, odd non-image responses) is transient/infra and
/// keeps the plain note. Pure, so the branching is unit-testable without a
/// network call.
pub(crate) fn classify_render_failure(status: u16, body: &str) -> MermaidResult {
    if (400..500).contains(&status) && !matches!(status, 408 | 429) {
        MermaidResult::ParseError(error_note(status, body))
    } else {
        MermaidResult::Failed(error_note(status, body))
    }
}

/// Whether an HTTP response represents a usable rendered image. Split out so
/// the accept/reject branching is unit-testable without a network call.
pub(crate) fn is_image_response(status: u16, content_type: &str) -> bool {
    (200..300).contains(&status) && content_type.to_lowercase().starts_with("image/")
}

/// Build a short, legible failure note from the renderer's response body.
pub(crate) fn error_note(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return format!("diagram renderer returned HTTP {status}");
    }
    trimmed.chars().take(ERROR_NOTE_MAX_CHARS).collect()
}

/// Pure: given a pre-validation outcome, the fence's position, and its
/// source, produce the markdown replacement and (for a valid image) the
/// media entry. Split out so the replacement shape is unit-testable without
/// a network call. `index` is the fence's ordinal in the message.
pub(crate) fn replacement_for(
    outcome: &MermaidResult,
    index: usize,
    source: &str,
    style: &MermaidStyle,
) -> (String, Option<MediaEntry>) {
    match outcome {
        MermaidResult::Image(url) => {
            let id = format!("diag{index}");
            // URL path does not carry byte dimensions locally; always provide vector link for URL-based delivery
            let md = format!("![diagram](tg://photo?id={id}){}", svg_link_md(style, source));
            (
                md,
                Some(MediaEntry {
                    id,
                    url: Some(url.clone()),
                    bytes: None,
                }),
            )
        }
        MermaidResult::ImageBytes(bytes) => {
            let id = format!("diag{index}");
            let capped = png_dims(bytes)
                .map(|(w, h)| is_diagram_capped(w, h))
                .unwrap_or(false);
            let md = if capped {
                format!("![diagram](tg://photo?id={id}){}", svg_link_md(style, source))
            } else {
                format!("![diagram](tg://photo?id={id})")
            };
            (
                md,
                Some(MediaEntry {
                    id,
                    url: None,
                    bytes: Some(bytes.clone()),
                }),
            )
        }
        // #189: a transient failure keeps the svg hatch — the render may well
        // exist server-side; a deterministic parse rejection does not, because
        // the renderer produced no diagram to link to.
        MermaidResult::Failed(err) => (markdown_failure_block_with_link(err, source, style), None),
        MermaidResult::ParseError(err) => (markdown_failure_block(err, source), None),
    }
}

/// Resolve a single fence to a render outcome. Remote-only delivery
/// (#1044 bytes path, #1238 ladder): mermaid.ink renders (ELK layout,
/// Chromium text rendering, full mermaid 11 diagram coverage) and the PNG
/// bytes ride to Telegram via multipart (`attach://`) — Telegram never
/// fetches a URL. On total failure the prevalidation note degrades to the
/// legible failure block; the in-process renderer is never invoked in
/// delivery.
async fn resolve_fence(style: &MermaidStyle, source: &str) -> MermaidResult {
    resolve(style, source).await
}

/// Resolve every mermaid fence in `text` for the markdown+media path: valid
/// diagrams become `![diagram](tg://photo?id=diagN)` references with a
/// matching [`MediaEntry`], broken ones become legible markdown failure
/// blocks. Non-fence text is untouched (byte-identical). Boxed because the
/// resolver is async and because it spans an await boundary.
pub(crate) fn resolve_markdown_media(text: &str) -> BoxFuture<'static, (String, Vec<MediaEntry>)> {
    let text = text.to_string();
    async move {
        let fences = find_mermaid_fences(&text);
        if fences.is_empty() {
            return (text, Vec::new());
        }
        // Resolve the style ONCE for the whole message (#318): every fence in
        // one message shares the same theme and surface, so re-resolving per
        // fence would only re-read the config.
        let style = MermaidStyle::from_config();
        let mut result = text.clone();
        let mut media = Vec::new();
        // Replace from last to first so earlier byte offsets stay valid.
        for (i, fence) in fences.iter().enumerate().rev() {
            let outcome = resolve_fence(&style, &fence.source).await;
            let (replacement, entry) = replacement_for(&outcome, i, &fence.source, &style);
            if let Some(e) = entry {
                media.push(e);
            }
            result.replace_range(fence.start..fence.end, &replacement);
        }
        // Media was pushed in reverse fence order; restore fence order.
        media.reverse();
        (result, media)
    }
    .boxed()
}

/// Media tag names Telegram's rich-markdown parser resolves as media entities.
/// A LIVE tag from model-authored prose is a whole-message rejection when its
/// source cannot be resolved against the media array (live Bot API probes,
/// 2026-09-11): bare `<img>` → `RICH_MESSAGE_PHOTO_INVALID`,
/// `<img src="tg://photo?id=x">` → same, `<video src="tg://video?id=v">` →
/// `RICH_MESSAGE_VIDEO_INVALID`, `<audio src="tg://audio?id=a">` →
/// `RICH_MESSAGE_AUDIO_INVALID`. `<iframe>` and `<img src="https://…">` pass.
/// Code-span quoting is NOT a safe hiding place: one unbalanced backtick in
/// prose shifts span pairing for the rest of the section and exposes the tag
/// (#134 root cause — a lone `<img>` in a 18.5K card body 400'd every send).
const PROSE_MEDIA_TAGS: [&str; 3] = ["img", "video", "audio"];

/// Escape the tag opener of every media tag in model-authored prose so the
/// text still READS `<img>` but can never go live (see [`PROSE_MEDIA_TAGS`]).
/// Markdown syntax is untouched — only the `<` of a media tag is rewritten.
pub(crate) fn neutralize_prose_media_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('<') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + 1..];
        let bytes = tail.as_bytes();
        let mut n = usize::from(bytes.first() == Some(&b'/'));
        let name_start = n;
        while n < bytes.len() && bytes[n].is_ascii_alphabetic() {
            n += 1;
        }
        let is_media = n > name_start
            && PROSE_MEDIA_TAGS
                .iter()
                .any(|t| tail[name_start..n].eq_ignore_ascii_case(t));
        out.push_str(if is_media { "&lt;" } else { "<" });
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Scheme prefixes that make Telegram try to RESOLVE a media reference, paired with
/// the non-triggering form each is rewritten to when its id is absent from `media`.
/// The rewrite drops the `//` after the scheme name, which no trigger prefix contains —
/// so a second pass is a no-op (idempotent) and the text stays readable.
const ORPHAN_MEDIA_SCHEMES: [(&str, &str); 4] = [
    ("tg://photo?id=", "tg:photo?id="),
    ("tg://video?id=", "tg:video?id="),
    ("tg://audio?id=", "tg:audio?id="),
    ("attach://", "attach:"),
];

/// Rewrite a media reference in an already-resolved rich body whose id has no
/// matching [`MediaEntry`] into its non-triggering form (probe-verified 200,
/// 2026-09-11) — see [`ORPHAN_MEDIA_SCHEMES`] for the four forms covered. Telegram
/// rejects the WHOLE message with `RICH_MESSAGE_PHOTO_INVALID` /
/// `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND` when a reference cannot be resolved, and prose
/// can carry such a reference as an example of the construct. `media` is the array of
/// the request this text belongs to; a reference naming an entry in it is left alone.
/// Sibling of [`neutralize_prose_media_html`], which closes the HTML-tag hole.
pub(crate) fn neutralize_orphan_photo_refs(text: &str, media: &[MediaEntry]) -> String {
    if !ORPHAN_MEDIA_SCHEMES
        .iter()
        .any(|(prefix, _)| text.contains(prefix))
    {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        // The EARLIEST match across all schemes wins, so text is always emitted in
        // order and a later scheme can never be rewritten ahead of an earlier one.
        let hit = ORPHAN_MEDIA_SCHEMES
            .iter()
            .filter_map(|(prefix, replacement)| {
                rest.find(prefix).map(|pos| (pos, *prefix, *replacement))
            })
            .min_by_key(|(pos, _, _)| *pos);
        let Some((pos, prefix, replacement)) = hit else {
            break;
        };
        out.push_str(&rest[..pos]);
        let after = &rest[pos + prefix.len()..];
        let id_len = after
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
            .count();
        let id = &after[..id_len];
        if !id.is_empty() && media.iter().any(|m| m.id == id) {
            out.push_str(prefix);
        } else {
            out.push_str(replacement);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Every media reference in `text` whose id has no matching entry in `media` —
/// the references a rich rejection is about (#334, H4). Shares
/// [`ORPHAN_MEDIA_SCHEMES`] with [`neutralize_orphan_photo_refs`], so the
/// extractor and the neutralizer can never disagree about what a media reference
/// is. Sorted and deduped so a log line is stable across runs.
///
/// Test-only since #334: the live caller ([`super::api::rich_body_offenders`])
/// holds ids parsed out of a request body, so it uses [`unresolved_media_refs_by`].
/// Kept compiled for tests so both forms stay covered and provably agree.
#[cfg(test)]
pub(crate) fn unresolved_media_refs(text: &str, media: &[MediaEntry]) -> Vec<String> {
    unresolved_media_refs_by(text, |id| media.iter().any(|m| m.id == id))
}

/// [`unresolved_media_refs`] against a bare id set, for callers that hold the ids
/// parsed out of a request body rather than a [`MediaEntry`] slice.
pub(crate) fn unresolved_media_refs_by(text: &str, is_known: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (prefix, _) in ORPHAN_MEDIA_SCHEMES {
        let mut rest = text;
        while let Some(pos) = rest.find(prefix) {
            let after = &rest[pos + prefix.len()..];
            let id_len = after
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
                .count();
            let id = &after[..id_len];
            if id.is_empty() || !is_known(id) {
                out.push(format!("{prefix}{id}"));
            }
            rest = after;
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Recursively replace every `Code{lang:"mermaid"}` block with a
/// `Mermaid{source, result}` block by pre-validating each fence. Handles
/// top-level fences and fences nested inside quotes, list items, and details.
/// Boxed because the walk is recursive and an async fn cannot recurse
/// without indirection (E0733). Used by the HTML fallback path.
pub(crate) fn resolve_blocks(blocks: Vec<Block>) -> BoxFuture<'static, Vec<Block>> {
    async move {
        // One style for the whole block tree (#318) — same reasoning as
        // resolve_markdown_media: every fence in one message shares a theme.
        let style = MermaidStyle::from_config();
        let mut out = Vec::with_capacity(blocks.len());
        for block in blocks {
            out.push(resolve_block(&style, block).await);
        }
        out
    }
    .boxed()
}

fn resolve_block(style: &MermaidStyle, block: Block) -> BoxFuture<'static, Block> {
    // The style is borrowed only for this call; the future is boxed for the
    // recursive walk, so clone the (small) style into it.
    let style = style.clone();
    async move {
        match block {
            Block::Code { lang, text }
                if lang.as_deref().is_some_and(is_mermaid_lang)
                    || (lang.is_none() && looks_like_mermaid_source(&text)) =>
            {
                let result = resolve_fence(&style, &text).await;
                Block::Mermaid {
                    source: text,
                    result,
                }
            }
            Block::Quote(inner) => Block::Quote(resolve_blocks(inner).await),
            Block::List(mut list) => {
                for item in &mut list.items {
                    item.children = resolve_blocks(std::mem::take(&mut item.children)).await;
                }
                Block::List(list)
            }
            Block::Details {
                summary,
                blocks,
                open,
            } => Block::Details {
                summary,
                blocks: resolve_blocks(blocks).await,
                open,
            },
            other => other,
        }
    }
    .boxed()
}

fn is_mermaid_lang(lang: &str) -> bool {
    lang.trim().eq_ignore_ascii_case("mermaid")
}

/// Markdown for a diagram that could not be rendered: a bold warning line,
/// then a code fence holding the renderer's error note and the original
/// source so the reader can see (and fix) what failed.
pub(crate) fn markdown_failure_block(err: &str, source: &str) -> String {
    format!(
        "> ⚠️ **Mermaid diagram could not be rendered**\n\n```\n{err}\n\nSource:\n{source}\n```"
    )
}

/// #189: the markdown failure block plus a `[svg]` escape hatch, for a
/// TRANSIENT failure ([`MermaidResult::Failed`]) only. In that case the
/// response had already passed the `2xx + image/*` check before the body was
/// lost, so the render very likely exists server-side and the link is worth
/// offering. A [`MermaidResult::ParseError`] is a deterministic rejection of
/// this exact source — the renderer produced no diagram, so the link would be
/// dead and the plain block stays. The URL is build-only: the browser does the
/// fetching, this host never touches mermaid.ink for it.
pub(crate) fn markdown_failure_block_with_link(
    err: &str,
    source: &str,
    style: &MermaidStyle,
) -> String {
    format!(
        "{}\n\n[svg]({})",
        markdown_failure_block(err, source),
        ink_url_svg(style, source)
    )
}

/// HTML for a successfully rendered diagram: a bare `<img>` in a `<figure>`,
/// which the Telegram rich-HTML parser turns into a native photo block.
pub(crate) fn image_html(url: &str) -> String {
    format!("<figure><img src=\"{}\"/></figure>", escape_html(url))
}

/// #134: rendered-image NOTE builder — NOT a failure: the diagram DID
/// render (owner directive 2026-09-10 03:56Z: a successful render is
/// never shown as an error). Legible explanation + raw source, with the
/// caller appending [`svg_link_html`] for the full-size vector link.
pub(crate) fn rendered_image_note(message: &str, source: &str) -> String {
    format!(
        "<b>🖼️ Diagram rendered as image</b>\n<blockquote>{}</blockquote>\n<pre><code>{}</code></pre>",
        escape_html(message),
        escape_html(source)
    )
}

/// #134 / #220 / #239: generic svg escape-hatch link fragment for markdown contexts —
/// a small `[Open SVG vector]` link to the full-size vector render when the diagram
/// is capped or scaled down. Always padded with trailing newline so subsequent markdown
/// blocks (e.g. tables, headers) start on a fresh line and retain proper block spacing.
pub(crate) fn svg_link_md(style: &MermaidStyle, source: &str) -> String {
    format!("\n[Open SVG vector]({})\n", ink_url_svg(style, source))
}

/// #134: generic svg escape-hatch link fragment for HTML-fallback
/// contexts — a small `[svg]` anchor to the vector render (generic
/// hatch; the caller owns the trigger copy, ruling (a) 2026-09-10:
/// ONE semantic — generic hatch here, caller-side trigger).
pub(crate) fn svg_link_html(style: &MermaidStyle, source: &str) -> String {
    format!(
        "\n<a href=\"{}\">[svg]</a>",
        escape_html(&ink_url_svg(style, source))
    )
}

/// HTML for a diagram that could not be rendered: a bold warning line, the
/// renderer's error note in a blockquote, and the original source in a code
/// block so the reader can see (and fix) what failed.
pub(crate) fn failure_html(err: &str, source: &str) -> String {
    format!(
        "<b>⚠️ Mermaid diagram could not be rendered</b>\n<blockquote>{}</blockquote>\n<pre><code>{}</code></pre>",
        escape_html(err),
        escape_html(source)
    )
}
