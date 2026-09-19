//! Seam tests for the channel-agnostic Mermaid validation core (#326).
//!
//! The seam is a pass-through: with no renderer installed it is a documented
//! no-op, with one installed it returns that renderer's verdict verbatim. The
//! probe-explicit core is exercised directly so the assertions never depend on
//! global state (no test-order coupling).

use crate::utils::mermaid::*;
use futures::future::BoxFuture;

/// Stand-in renderer returning a fixed verdict, so these tests assert the
/// SEAM's behaviour (pass-through vs no-op) without touching the network or
/// the global registry.
struct FakeProbe(Vec<String>);

impl MermaidProbe for FakeProbe {
    fn validate<'a>(&'a self, _text: &'a str) -> BoxFuture<'a, Vec<String>> {
        Box::pin(async move { self.0.clone() })
    }
}

#[tokio::test]
async fn no_probe_is_a_no_op() {
    let text = "```mermaid\ngraph TD;\nA-->B\n```";
    assert!(validate_with(None, text).await.is_empty());
}

#[tokio::test]
async fn installed_probe_verdict_passes_through() {
    let probe = FakeProbe(vec!["line 3: unexpected token".to_string()]);
    let errors = validate_with(Some(&probe), "```mermaid\ngraph TD;\nA-->B\n```").await;
    assert_eq!(errors, vec!["line 3: unexpected token".to_string()]);
}

#[tokio::test]
async fn clean_parse_reports_nothing_to_fix() {
    let probe = FakeProbe(Vec::new());
    let errors = validate_with(Some(&probe), "```mermaid\ngraph TD;\n```").await;
    assert!(errors.is_empty());
}

#[test]
fn format_mermaid_error_names_the_context() {
    let err = format_mermaid_error("plan markdown", &["boom".to_string()]);
    assert!(err.starts_with("Mermaid diagram syntax error in plan markdown."));
    assert!(err.contains("boom"));
    assert!(err.contains("Correction rules:"));
}

#[test]
fn fence_parser_locates_a_tagged_mermaid_block() {
    let text = "intro\n```mermaid\ngraph TD\nA-->B\n```\noutro";
    assert!(has_mermaid_fence(text));
    let fences = find_mermaid_fences(text);
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].source.trim(), "graph TD\nA-->B");
    assert!(looks_like_mermaid_source(&fences[0].source));
}
