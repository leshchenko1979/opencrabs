//! Tests for named HTML entity decoding in Telegram markdown / rich rendering (#258).

use crate::channels::telegram::markdown::{
    decode_named_entities, escape_html, markdown_to_telegram_html, md_to_html, unescape_html,
};
use crate::channels::telegram::rich::markdown_to_html;

#[test]
fn test_decode_named_entities_basic_arrows() {
    assert_eq!(
        decode_named_entities("CPU: 8.01 &rarr; 6.80 &rarr; 2.52"),
        "CPU: 8.01 → 6.80 → 2.52"
    );
    assert_eq!(
        decode_named_entities("&larr; left &uarr; up &darr; down &rArr; double &harr; both"),
        "← left ↑ up ↓ down ⇒ double ↔ both"
    );
}

#[test]
fn test_decode_named_entities_bullets_and_punctuation() {
    assert_eq!(
        decode_named_entities("&bull; Item 1 &mdash; details &hellip;"),
        "• Item 1 — details …"
    );
    assert_eq!(
        decode_named_entities(
            "&middot; middle &ndash; en &nbsp; space &para; pilcrow &sect; section"
        ),
        "· middle – en \u{00A0} space ¶ pilcrow § section"
    );
    assert_eq!(
        decode_named_entities(
            "&lsquo;single&rsquo; and &ldquo;double&rdquo; quotes &laquo;guillemets&raquo;"
        ),
        "‘single’ and “double” quotes «guillemets»"
    );
}

#[test]
fn test_decode_named_entities_math_and_symbols() {
    assert_eq!(
        decode_named_entities(
            "&check; pass &cross; fail &plusmn; 5 &ne; 0 &le; 10 &ge; 2 &asymp; 3.14"
        ),
        "✓ pass ✗ fail ± 5 ≠ 0 ≤ 10 ≥ 2 ≈ 3.14"
    );
    assert_eq!(
        decode_named_entities(
            "&times; multiply &divide; divide &deg; 100 &copy; 2026 &reg; &trade;"
        ),
        "× multiply ÷ divide ° 100 © 2026 ® ™"
    );
    assert_eq!(
        decode_named_entities("&hearts; &diams; &clubs; &spades; &star;"),
        "♥ ♦ ♣ ♠ ★"
    );
}

#[test]
fn test_preserve_xml_and_numeric_entities() {
    // Crucial: &lt;, &gt;, &amp;, &quot;, &apos; must NOT be decoded, as they are core XML syntax entities
    assert_eq!(
        decode_named_entities("&lt;b&gt;bold&lt;/b&gt; &amp; &quot;quote&quot; &apos;apos&apos;"),
        "&lt;b&gt;bold&lt;/b&gt; &amp; &quot;quote&quot; &apos;apos&apos;"
    );
    // Numeric character references must stay untouched
    assert_eq!(
        decode_named_entities("&#123; &#x1F600; &#60;"),
        "&#123; &#x1F600; &#60;"
    );
}

#[test]
fn test_unescaped_ampersands_and_unknown_entities() {
    // Unescaped ampersands like AT&T or & alone must remain untouched
    assert_eq!(
        decode_named_entities("AT&T and Ben & Jerry's &"),
        "AT&T and Ben & Jerry's &"
    );
    // Unknown or incomplete entities must not crash or corrupt
    assert_eq!(
        decode_named_entities("&unknown; &rarr without semicolon &"),
        "&unknown; &rarr without semicolon &"
    );
    assert_eq!(
        decode_named_entities("Double &&rarr;; test"),
        "Double &→; test"
    );
}

#[test]
fn test_markdown_to_telegram_html_integration() {
    let input =
        "Load receding: `8.01` &rarr; `6.80` &rarr; `2.52`\n- &check; Disk OK\n- &cross; High RAM";
    let rendered = markdown_to_telegram_html(input);
    assert!(rendered.contains("→"));
    assert!(rendered.contains("✓"));
    assert!(rendered.contains("✗"));
    assert!(!rendered.contains("&rarr;"));
    assert!(!rendered.contains("&check;"));
    assert!(!rendered.contains("&cross;"));
}

#[test]
fn test_rich_markdown_to_html_integration() {
    let input = "## System Status\n\n| Metric | Before &rarr; After | Status |\n|---|---|---|\n| CPU | 8.0 &rarr; 2.5 | &check; OK |\n";
    let rendered = markdown_to_html(input);
    assert!(rendered.contains("→"));
    assert!(rendered.contains("✓"));
    assert!(!rendered.contains("&rarr;"));
    assert!(!rendered.contains("&check;"));
}

#[test]
fn test_md_to_html_integration() {
    let input = "**Alert**: CPU load &rarr; critical (&bull; note)";
    let rendered = md_to_html(input);
    assert!(rendered.contains("<b>Alert</b>: CPU load → critical (• note)"));
}

#[test]
fn test_unescape_html_round_trips_escape_html() {
    // unescape_html is the exact inverse of escape_html, so a round trip must
    // be the identity for every input — including the entity-lookalike strings
    // that a naive "decode everything" implementation would corrupt.
    let cases = [
        "Acknowledge & stamp gap closed",
        "AT&T",
        "Tom & Jerry <3",
        "<b>bold</b>",
        "&amp;",
        "&lt;",
        "&gt;",
        "&amp;lt;",
        "&#65;",
        "&quot;",
        "&apos;",
        "&rarr;",
        "a & b & c",
        "",
        "no entities at all",
        "&&&&",
    ];
    for s in cases {
        assert_eq!(
            unescape_html(&escape_html(s)),
            s,
            "round trip failed for {s:?}"
        );
    }
}

#[test]
fn test_unescape_html_decodes_amp_last() {
    // Order is load-bearing: escape_html escapes '&' FIRST, so a literal
    // "&lt;" in the source is stored as "&amp;lt;". Decoding "&amp;" before
    // "&lt;" would yield "<" instead of the literal "&lt;" the author wrote.
    assert_eq!(escape_html("&lt;"), "&amp;lt;");
    assert_eq!(unescape_html("&amp;lt;"), "&lt;");
    assert_eq!(escape_html("&amp;"), "&amp;amp;");
    assert_eq!(unescape_html("&amp;amp;"), "&amp;");
}

#[test]
fn test_unescape_html_leaves_foreign_entities_verbatim() {
    // Scope is limited to the three entities escape_html can emit. Anything
    // else must survive untouched — otherwise hand-authored labels from other
    // lanes get silently rewritten.
    assert_eq!(
        unescape_html("&#65; &quot;q&quot; &apos;a&apos;"),
        "&#65; &quot;q&quot; &apos;a&apos;"
    );
    assert_eq!(unescape_html("&rarr; &bull; &mdash;"), "&rarr; &bull; &mdash;");
    assert_eq!(unescape_html("&unknown; &"), "&unknown; &");
}

#[test]
fn test_unescape_html_restores_owner_label_display_width() {
    // The defect this guards (#396): the escaped form of the owner's label is
    // 34 characters, over the 30-unit solo button budget, while its display
    // text is 30 and fits. Measuring the escaped form folded the button to a
    // bare digit.
    let label = "Acknowledge & stamp gap closed";
    let escaped = escape_html(label);
    assert_eq!(label.chars().count(), 30);
    assert_eq!(escaped.chars().count(), 34);
    assert_eq!(unescape_html(&escaped).chars().count(), 30);
    assert_eq!(unescape_html(&escaped), label);
}
