//! Spill preview byte cap (#1001).

use crate::brain::agent::service::tool_loop::{
    DEFAULT_MAX_INLINE_TOOL_BYTES, build_head_tail_preview, cap_inline_bytes,
};

#[test]
fn head_tail_preview_over_eighty_lines_respects_byte_cap() {
    let line = "W".repeat(500);
    let body: String = (0..100)
        .map(|_| line.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(body.lines().count() > 80);
    let preview = build_head_tail_preview(&body, DEFAULT_MAX_INLINE_TOOL_BYTES);
    assert!(
        preview.len() <= DEFAULT_MAX_INLINE_TOOL_BYTES,
        "preview was {} bytes",
        preview.len()
    );
    assert!(preview.contains("lines omitted"));
}

#[test]
fn cap_inline_bytes_respects_utf8_boundaries() {
    let s = "é".repeat(100);
    let capped = cap_inline_bytes(&s, 10);
    assert!(capped.len() <= 10 + 64);
}
