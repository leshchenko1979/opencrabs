//! Tests for the themed mermaid render path (#318).
//!
//! Pure, network-free coverage of [`MermaidStyle`]: the query string, the
//! base64url JSON-state payload, the render-cache key, and the colour
//! conversions the style is built from. Nothing here performs HTTP — the
//! payload is decoded and parsed in-process, which is the point: a regression
//! to the plain-code payload form, or to the JSON-state form with
//! `themeVariables` hoisted OUT of the `mermaid` object (mermaid.ink silently
//! discards it there — see [`MermaidStyle::payload`]), or a dropped palette
//! key fails HERE,
//! rather than on the wire where it surfaces only as a mis-coloured diagram in
//! a Telegram client.
//!
//! ## Determinism vs the process-global active theme
//!
//! [`MermaidStyle::from_values`] resolves `auto` against `theme::active()`, and
//! `tui_theme_presets_test` mutates that global slot. Every `src/tests/*`
//! module shares ONE test binary and runs in parallel threads, so an assertion
//! that reads the active theme can race a concurrent `set`. Two defences, in
//! order of preference:
//!
//! 1. Most cases pin `theme` AND `bg` explicitly, so the active slot cannot
//!    reach them at all. Note that `bg` does NOT pin the palette — the palette
//!    is derived from the active theme's own surface — so palette values are
//!    asserted as set membership or against the style's own fields.
//! 2. The few cases that must exercise `auto` run inside
//!    [`with_stable_theme`], a seqlock over `theme::generation()`: if a `set`
//!    lands mid-read the comparison is retried, so a race cannot report a
//!    failure nothing caused. The assertion itself stays OUTSIDE the retried
//!    closure — a panic inside it would escape before the retry could withdraw
//!    the result.

use crate::channels::telegram::rich::mermaid::MermaidStyle;
use crate::tui::render::theme;
use base64::Engine as _;
use ratatui::style::Color;

/// The palette keys mermaid.ink actually honours, each proven by a single-key
/// wire probe against a fixed baseline (base md5 `5780049e88a8`). A payload
/// carrying fewer has dropped a palette key.
const HONOURED_KEYS: [&str; 6] = [
    "lineColor",
    "nodeBorder",
    "mainBkg",
    "clusterBkg",
    "clusterBorder",
    "textColor",
];

/// The five probed-and-inert keys. Asserted ABSENT so a later "let's send the
/// whole palette" change cannot quietly re-inflate every payload with keys
/// that moved zero pixels on the wire.
const INERT_KEYS: [&str; 5] = [
    "primaryColor",
    "primaryBorderColor",
    "secondaryColor",
    "background",
    "edgeLabelBackground",
];

/// Re-run `body` until the active theme held still across it, then return its
/// result.
///
/// [`MermaidStyle::from_values`] resolves `auto` against `theme::active()`, and
/// `tui_theme_presets_test` mutates that global slot. Every `src/tests/*`
/// module shares ONE test binary and runs in parallel threads, so a case that
/// compares two `auto` resolutions can straddle a concurrent `set` and compare
/// values derived from different themes. Retrying until the generation holds
/// still makes such a comparison deterministic instead of flaky — and it must
/// be a RETRY rather than a skip, because the comparison itself panics on
/// mismatch, leaving no point after the fact at which a failure could be
/// withdrawn.
fn with_stable_theme<T>(mut body: impl FnMut() -> T) -> T {
    for _ in 0..8 {
        let before = theme::generation();
        let out = body();
        if theme::generation() == before {
            return out;
        }
    }
    // The mutator never settles (in practice it runs only a handful of times):
    // take the last reading rather than spin forever.
    body()
}

/// The bare lowercase 6-hex `bgColor` value mermaid.ink accepts for a colour.
fn bare(color: Color) -> String {
    theme::hex_of(color)
        .expect("test colours are Rgb")
        .trim_start_matches('#')
        .to_ascii_lowercase()
}

/// The `bgColor` value `auto` resolves to for the CURRENTLY active theme.
fn active_surface_bare() -> String {
    match theme::hex_of(theme::active().colors.surface_code) {
        Some(hex) => hex.trim_start_matches('#').to_ascii_lowercase(),
        // No portable hex: `MermaidStyle` falls back to the crab-dark surface.
        None => "282d37".to_string(),
    }
}

/// Decode a style's payload back into the JSON state object it encodes.
fn decode_payload(style: &MermaidStyle, source: &str) -> serde_json::Value {
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(style.payload(source).as_bytes())
        .expect("payload must be base64url");
    serde_json::from_slice(&raw).expect("payload must decode to JSON, not plain code")
}

// ---------------------------------------------------------------------------
// query()
// ---------------------------------------------------------------------------

#[test]
fn query_without_bg_is_the_bare_png_type() {
    // `none` restores the pre-#318 transparent look, so no bgColor at all.
    assert_eq!(
        MermaidStyle::from_values("dark", "none").query(""),
        "?type=png"
    );
}

#[test]
fn query_with_an_explicit_hex_carries_it_bare_and_lowercase() {
    // Bare is load-bearing: a `#`-prefixed bgColor is silently ignored on the
    // wire, which is the original defect wearing a disguise.
    assert_eq!(
        MermaidStyle::from_values("dark", "#282D37").query(""),
        "?type=png&bgColor=282d37"
    );
    assert_eq!(
        MermaidStyle::from_values("dark", "AABBCC").query(""),
        "?type=png&bgColor=aabbcc"
    );
}

#[test]
fn query_appends_extra_after_the_bg() {
    assert_eq!(
        MermaidStyle::from_values("dark", "#282d37").query("width=1200"),
        "?type=png&bgColor=282d37&width=1200"
    );
    assert_eq!(
        MermaidStyle::from_values("dark", "none").query("width=1200"),
        "?type=png&width=1200"
    );
}

#[test]
fn query_is_independent_of_the_glyph_palette() {
    // `theme` rides in the payload, never the query: the same surface must
    // produce the same query under every palette, or the "Open SVG vector"
    // link and the embedded PNG could ask for two different renders.
    let pinned = MermaidStyle::from_values("dark", "#282d37").query("width=1200");
    for name in ["default", "neutral", "forest", "base", "auto"] {
        assert_eq!(
            MermaidStyle::from_values(name, "#282d37").query("width=1200"),
            pinned,
            "palette {name:?} changed the query string"
        );
    }
}

#[test]
fn query_auto_bg_uses_the_active_surface() {
    let all_agree = with_stable_theme(|| {
        let expected = format!("?type=png&bgColor={}", active_surface_bare());
        // Blank and a case variant must mean the same thing as `auto`.
        ["auto", "  ", "AUTO"]
            .iter()
            .all(|cfg| MermaidStyle::from_values("dark", cfg).query("") == expected)
    });
    assert!(
        all_agree,
        "`auto` — and its blank and case variants — must resolve to the active \
         theme's surface"
    );
}

#[test]
fn query_invalid_bg_values_fall_back_to_the_active_surface() {
    // The shapes a config typo realistically produces: a percent-encoded URL
    // form, a CSS colour name, and wrong lengths or digit counts.
    const BAD: [&str; 8] = [
        "%23ff0000", // percent-encoded '#': a URL form, not a hex value
        "lightgrey", // named colour: mermaid.ink wants hex
        "12345",     // five digits
        "#12345",    // five digits, prefixed
        "gggggg",    // six non-hex characters
        "#ff000",    // too short
        "ff0000ff",  // eight digits (alpha): not a mermaid bgColor
        "#",         // just the marker
    ];
    let all_fall_back = with_stable_theme(|| {
        let auto = MermaidStyle::from_values("dark", "auto").query("");
        BAD.iter()
            .all(|bad| MermaidStyle::from_values("dark", bad).query("") == auto)
    });
    assert!(
        all_fall_back,
        "an invalid mermaid_bg must fall back to the active surface — silently \
         accepting it would ship a transparent PNG, the exact defect #318 removes"
    );
}

// ---------------------------------------------------------------------------
// theme resolution
// ---------------------------------------------------------------------------

#[test]
fn theme_known_names_pass_through_case_insensitively() {
    for name in ["default", "dark", "neutral", "forest", "base"] {
        assert_eq!(MermaidStyle::from_values(name, "none").theme, name);
        let shouted = name.to_ascii_uppercase();
        assert_eq!(
            MermaidStyle::from_values(&shouted, "none").theme,
            name,
            "a case variant must resolve to the canonical name"
        );
    }
}

#[test]
fn theme_auto_resolves_to_one_of_the_two_luminance_palettes() {
    // Which one depends on the active surface, so assert the SET: a value
    // outside it would mean `auto` stopped tracking luminance altogether.
    for cfg in ["auto", "AUTO", "", "   "] {
        let resolved = MermaidStyle::from_values(cfg, "none").theme;
        assert!(
            resolved == "dark" || resolved == "default",
            "mermaid_theme {cfg:?} resolved to {resolved:?}, which is neither \
             luminance palette"
        );
    }
}

#[test]
fn theme_invalid_values_fall_back_to_the_auto_palette() {
    const BAD: [&str; 6] = ["#282d37", "lightgrey", "12345", "bogus", "dark-ish", "nope"];
    let all_fall_back = with_stable_theme(|| {
        let auto = MermaidStyle::from_values("auto", "none").theme;
        BAD.iter()
            .all(|bad| MermaidStyle::from_values(bad, "none").theme == auto)
    });
    assert!(
        all_fall_back,
        "an unrecognised mermaid_theme must fall back to the auto palette"
    );
}

// ---------------------------------------------------------------------------
// cache_key()
// ---------------------------------------------------------------------------

#[test]
fn cache_key_differs_across_themes_for_one_identical_source() {
    let src = "graph TD;\nA-->B;";
    assert_ne!(
        MermaidStyle::from_values("dark", "#282d37").cache_key(src),
        MermaidStyle::from_values("default", "#282d37").cache_key(src),
        "a render cached under one palette would be served for another"
    );
}

#[test]
fn cache_key_differs_across_backgrounds_for_one_identical_source() {
    let src = "graph TD;\nA-->B;";
    assert_ne!(
        MermaidStyle::from_values("dark", "#282d37").cache_key(src),
        MermaidStyle::from_values("dark", "none").cache_key(src)
    );
    assert_ne!(
        MermaidStyle::from_values("dark", "#282d37").cache_key(src),
        MermaidStyle::from_values("dark", "#101010").cache_key(src)
    );
}

#[test]
fn cache_key_is_stable_for_identical_inputs() {
    let src = "sequenceDiagram\n  A->>B: hi";
    assert_eq!(
        MermaidStyle::from_values("dark", "#282d37").cache_key(src),
        MermaidStyle::from_values("dark", "#282d37").cache_key(src)
    );
}

#[test]
fn cache_key_ignores_a_trailing_newline() {
    // The #100 normalisation, folded into the one key builder: a fence's
    // trailing newline must not mint a second cache entry for a diagram that
    // was already rendered.
    let style = MermaidStyle::from_values("dark", "#282d37");
    assert_eq!(style.cache_key("A-->B;"), style.cache_key("A-->B;\n"));
    assert_eq!(style.cache_key("A-->B;"), style.cache_key("A-->B;\n\n"));
}

#[test]
fn cache_key_differs_across_sources() {
    let style = MermaidStyle::from_values("dark", "#282d37");
    assert_ne!(style.cache_key("A-->B;"), style.cache_key("A-->C;"));
}

// ---------------------------------------------------------------------------
// colour conversions
// ---------------------------------------------------------------------------

#[test]
fn hex_of_formats_rgb_as_lowercase_six_hex() {
    assert_eq!(
        theme::hex_of(Color::Rgb(40, 45, 55)).as_deref(),
        Some("#282d37")
    );
    assert_eq!(theme::hex_of(Color::Rgb(0, 0, 0)).as_deref(), Some("#000000"));
    assert_eq!(
        theme::hex_of(Color::Rgb(255, 255, 255)).as_deref(),
        Some("#ffffff")
    );
    assert_eq!(
        theme::hex_of(Color::Rgb(10, 11, 12)).as_deref(),
        Some("#0a0b0c"),
        "channels below 0x10 must be zero-padded"
    );
    // The shape `bgColor` needs on the wire.
    assert_eq!(bare(Color::Rgb(40, 45, 55)), "282d37");
}

#[test]
fn hex_of_is_none_for_every_non_rgb_variant() {
    // A terminal resolves these against its own palette, so there is no
    // portable hex to report — the style falls back rather than guessing.
    for color in [
        Color::Reset,
        Color::Black,
        Color::White,
        Color::Red,
        Color::Indexed(5),
        Color::Indexed(255),
    ] {
        assert!(
            theme::hex_of(color).is_none(),
            "{color:?} must report no hex"
        );
    }
}

#[test]
fn relative_luminance_spans_zero_to_one_for_rgb() {
    assert_eq!(theme::relative_luminance(Color::Rgb(0, 0, 0)), 0.0);
    assert_eq!(theme::relative_luminance(Color::Rgb(255, 255, 255)), 1.0);
    // crab-dark's surface is dark; solarized-light's is light. The midpoint
    // test is what makes `auto` pick a palette, so pin both sides of it.
    let crab_dark = theme::relative_luminance(Color::Rgb(40, 45, 55));
    let solarized_light = theme::relative_luminance(Color::Rgb(253, 246, 227));
    assert!(crab_dark > 0.0 && crab_dark < 0.5, "got {crab_dark}");
    assert!(
        solarized_light > 0.5 && solarized_light < 1.0,
        "got {solarized_light}"
    );
}

#[test]
fn relative_luminance_is_zero_for_non_rgb() {
    // The documented conservative default: "dark", never a measurement.
    for color in [Color::Reset, Color::Indexed(5), Color::White] {
        assert_eq!(theme::relative_luminance(color), 0.0, "{color:?}");
    }
}

// ---------------------------------------------------------------------------
// palette
// ---------------------------------------------------------------------------

#[test]
fn palette_line_colour_is_one_of_the_two_designed_values() {
    // The #318 regression itself: mermaid's own dark palette draws connectors
    // `#d3d3d3` (9.22:1 on `#282d37`) and the owner reported the arrows getting
    // LESS visible. The style overrides that to pure white on a dark surface
    // (11.28:1 connector-only rows, 13.81:1 stroke) and near-black on a light
    // one. Anything else is the regression returning.
    let line = MermaidStyle::from_values("dark", "#282d37").palette.line.clone();
    assert!(
        line == "#ffffff" || line == "#333333",
        "connector colour {line} is neither of the two designed values"
    );
}

#[test]
fn palette_fills_are_valid_lowercase_hex_and_are_not_the_same_fill() {
    let palette = MermaidStyle::from_values("dark", "#282d37").palette;
    for (label, value) in [
        ("mainBkg", &palette.main_bkg),
        ("clusterBkg", &palette.cluster_bkg),
    ] {
        assert_eq!(value.len(), 7, "{label} must be `#rrggbb`, got {value}");
        assert!(value.starts_with('#'), "{label} must be `#`-prefixed");
        assert!(
            value[1..].chars().all(|c| c.is_ascii_hexdigit()),
            "{label} must be hex, got {value}"
        );
        assert_eq!(*value, value.to_ascii_lowercase(), "{label} must be lowercase");
    }
    // The node and subgraph offsets differ, so the two fills differ for every
    // surface a theme can actually resolve to: colliding needs all channels
    // clamped, which forces the opposite dark/light branch.
    assert_ne!(
        palette.main_bkg, palette.cluster_bkg,
        "node and subgraph fills must not collapse to one colour"
    );
}

// ---------------------------------------------------------------------------
// from_config()
// ---------------------------------------------------------------------------

#[test]
fn from_config_reads_the_live_telegram_config() {
    // The wiring guard: `from_config` must resolve through the SAME values a
    // direct `from_values` call would use. Asserted against the live config
    // rather than a hardcoded expectation, because the test environment's
    // config (the embedded example, or a dev's own config.toml) decides them.
    let tg = &crate::config::Config::current().channels.telegram;
    let expected = MermaidStyle::from_values(&tg.mermaid_theme, &tg.mermaid_bg);
    let actual = MermaidStyle::from_config();
    assert_eq!(actual.theme, expected.theme);
    assert_eq!(actual.bg, expected.bg);
    assert_eq!(actual.palette.line, expected.palette.line);
    assert_eq!(actual.palette.main_bkg, expected.palette.main_bkg);
}

#[test]
fn from_config_resolves_to_a_known_palette() {
    let style = MermaidStyle::from_config();
    assert!(
        ["default", "dark", "neutral", "forest", "base"].contains(&style.theme),
        "from_config produced an unrecognised palette {:?}",
        style.theme
    );
    assert!(
        style.query("").starts_with("?type=png"),
        "every request must be a png request, got {}",
        style.query("")
    );
}

#[test]
fn from_config_tracks_the_active_surface_when_left_on_auto() {
    let tg = &crate::config::Config::current().channels.telegram;
    if !(tg.mermaid_bg.trim().is_empty() || tg.mermaid_bg.eq_ignore_ascii_case("auto")) {
        // The environment's config pins a background, so `auto` is not in play.
        return;
    }
    let follows = with_stable_theme(|| {
        MermaidStyle::from_config().bg.as_deref() == Some(active_surface_bare().as_str())
    });
    assert!(
        follows,
        "the shipped `auto` default must follow the active surface, or a stock \
         install gets a background that does not match its theme"
    );
}

// ---------------------------------------------------------------------------
// payload()
// ---------------------------------------------------------------------------

#[test]
fn payload_decodes_to_the_json_state_object_with_the_source() {
    // The regression guard for the payload FORM: base64url of the raw source
    // is also accepted by mermaid.ink, and `themeVariables` is silently
    // ignored in that shape. Raw source is not JSON, so this decode-and-parse
    // fails the moment the payload reverts to the plain-code form.
    let style = MermaidStyle::from_values("dark", "#282d37");
    let src = "graph TD;\nA-->B;";
    let json = decode_payload(&style, src);
    assert_eq!(json["code"], src);
    assert!(json["mermaid"].is_object(), "mermaid must be an object");
    assert!(
        json["mermaid"]["themeVariables"].is_object(),
        "themeVariables must be an object INSIDE the mermaid config — \
         hoisted to the top level, mermaid.ink discards it without a warning"
    );
    assert!(
        json["themeVariables"].is_null(),
        "a top-level themeVariables is inert on the wire; the palette must \
         live under `mermaid`"
    );
}

#[test]
fn payload_carries_the_palette_name_inside_the_json() {
    for name in ["default", "dark", "neutral", "forest", "base"] {
        let style = MermaidStyle::from_values(name, "#282d37");
        let json = decode_payload(&style, "A-->B;");
        assert_eq!(
            json["mermaid"]["theme"], name,
            "the palette must ride in the JSON state, not the query"
        );
    }
}

#[test]
fn payload_theme_variables_carry_exactly_the_six_honoured_keys() {
    let style = MermaidStyle::from_values("dark", "#282d37");
    let json = decode_payload(&style, "A-->B;");
    let vars = json["mermaid"]["themeVariables"]
        .as_object()
        .expect("themeVariables must be an object");
    let mut got: Vec<&str> = vars.keys().map(String::as_str).collect();
    got.sort_unstable();
    let mut want = HONOURED_KEYS.to_vec();
    want.sort_unstable();
    assert_eq!(
        got, want,
        "the payload must carry exactly the honoured palette — a dropped key \
         is a silently unstyled element on the wire"
    );
}

#[test]
fn payload_never_carries_an_inert_key() {
    let style = MermaidStyle::from_values("dark", "#282d37");
    let json = decode_payload(&style, "A-->B;");
    let vars = json["mermaid"]["themeVariables"]
        .as_object()
        .expect("themeVariables must be an object");
    for key in INERT_KEYS {
        assert!(
            !vars.contains_key(key),
            "{key} moved zero pixels on the wire; emitting it only enlarges \
             every payload"
        );
    }
}

#[test]
fn payload_theme_variables_match_the_style_palette() {
    // The values are theme-derived, so assert them against the style's OWN
    // fields rather than hardcoded hex: that stays honest whatever surface the
    // active theme resolves to, while still catching a payload that drops,
    // blanks or transposes a key.
    let style = MermaidStyle::from_values("dark", "#282d37");
    let json = decode_payload(&style, "A-->B;");
    let vars = &json["mermaid"]["themeVariables"];
    assert_eq!(vars["lineColor"], style.palette.line);
    assert_eq!(vars["nodeBorder"], style.palette.node_border);
    assert_eq!(vars["mainBkg"], style.palette.main_bkg);
    assert_eq!(vars["clusterBkg"], style.palette.cluster_bkg);
    assert_eq!(vars["clusterBorder"], style.palette.cluster_border);
    assert_eq!(vars["textColor"], style.palette.text);
}

#[test]
fn payload_tracks_the_source_and_is_base64url() {
    let style = MermaidStyle::from_values("dark", "#282d37");
    assert_ne!(style.payload("A-->B;"), style.payload("A-->C;"));
    let p = style.payload("graph TD;\nA-->B;");
    assert!(
        !p.contains('+') && !p.contains('/') && !p.contains('='),
        "payload must be base64url with no padding, got {p}"
    );
}
