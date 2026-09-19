//! Channel-agnostic Mermaid output validation (#326).
//!
//! Fence detection, the shared correction rules and the error formatter live
//! here so core (`brain/`) and `utils/` can validate model output without
//! depending on a channel module. The dependency used to run the wrong way —
//! core pulled the helpers out of the Telegram channel's `rich` module — and
//! every core call site was `#[cfg(feature = "telegram")]`-gated, so a
//! telegram-less build silently lost the validation *and* its self-healing
//! regen nudge.
//!
//! Rendering stays channel-specific. A channel that can render diagrams
//! installs a [`MermaidProbe`] at construction ([`install_probe`]); callers ask
//! [`validate`]. With no renderer installed the seam is a documented no-op — a
//! deliberate behavioural difference from a compile-time deletion.

use futures::future::BoxFuture;
use std::sync::OnceLock;

/// A located ```mermaid fence in the source markdown. `start` is the byte
/// offset of the opening fence line's first byte; `end` is the byte offset
/// just past the closing fence line (including its terminator). Replacing
/// `text[start..end]` swaps the fence without touching the rest.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MermaidFence {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) source: String,
}

/// Whether a fence body (the text between opening and closing ``` lines)
/// starts like a mermaid diagram. Used to classify *untagged* fences, whose
/// info string is empty: models frequently emit diagrams as ```graph TD ...
/// without the `mermaid` tag. The first non-blank, non-comment (`%%`) line
/// must start with a known diagram opener; `graph` additionally requires a
/// direction word (`TD`/`TB`/`BT`/`LR`/`RL`) so DOT-style `graph G {` and
/// similar foreign notations are not misclassified.
pub(crate) fn looks_like_mermaid_source(source: &str) -> bool {
    for raw in source.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        let mut words = line.split_whitespace();
        let head = words.next().unwrap_or("").to_ascii_lowercase();
        return match head.as_str() {
            "graph" => words.next().is_some_and(|d| {
                matches!(
                    d.to_ascii_lowercase().as_str(),
                    "td" | "tb" | "bt" | "lr" | "rl"
                )
            }),
            "flowchart" | "sequencediagram" | "classdiagram" | "classdiagram-v2"
            | "statediagram" | "statediagram-v2" | "erdiagram" | "journey" | "gantt" | "pie"
            | "quadrantchart" | "requirementdiagram" | "gitgraph" | "mindmap" | "timeline"
            | "zenuml" | "sankey-beta" | "xychart-beta" | "block-beta" | "packet-beta"
            | "architecture-beta" => true,
            _ => false,
        };
    }
    false
}

/// Whether `text` contains a fence that should render as a mermaid diagram:
/// either tagged ```mermaid, or untagged with mermaid-shaped content
/// ([`looks_like_mermaid_source`]). A fast line-scan used to gate the richer
/// (async) render path.
pub(crate) fn has_mermaid_fence(text: &str) -> bool {
    let mut in_fence = false;
    let mut tagged_mermaid = false;
    // Whether the OPENING fence carried no info string. Content
    // classification applies to bare fences only, so the opening tag is what
    // decides it — the closing line is bare almost every time and says
    // nothing about the block.
    let mut untagged = false;
    let mut body_start = 0usize;
    let mut pos = 0usize;
    for line in text.split_inclusive('\n') {
        let line_end = pos + line.len();
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            // Tolerant pairing (14:23Z bug class): an info-carrying fence
            // line is ALWAYS an opener — it implicitly closes any open
            // block. Only a BARE fence line closes cleanly. A stray bare
            // fence before ```mermaid used to pair with the tagged opener,
            // desyncing the machine so the real diagram never located and
            // raw fences shipped.
            let info = rest.trim();
            if in_fence {
                let body = text[body_start..pos].trim_end_matches('\n');
                if tagged_mermaid || (untagged && looks_like_mermaid_source(body)) {
                    return true;
                }
            }
            in_fence = true;
            tagged_mermaid = info.eq_ignore_ascii_case("mermaid");
            untagged = info.is_empty();
            body_start = line_end;
        }
        pos = line_end;
    }
    false
}

/// Locate every mermaid fence in `text`, returning byte ranges and the
/// diagram source between the fences. Consistent with [`has_mermaid_fence`]:
/// a fence qualifies when its info string trims to `mermaid`
/// (case-insensitive), or is empty and the body starts like a diagram
/// ([`looks_like_mermaid_source`]); either way it is closed by the next
/// bare ``` line.
pub(crate) fn find_mermaid_fences(text: &str) -> Vec<MermaidFence> {
    let mut fences = Vec::new();
    let mut in_fence = false;
    let mut is_mermaid = false;
    // See `has_mermaid_fence`: classification keys off the OPENING info
    // string, never the closing line's.
    let mut untagged = false;
    let mut block_start = 0usize;
    let mut source_start = 0usize;
    let mut pos = 0usize;

    for line in text.split_inclusive('\n') {
        let line_end = pos + line.len();
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("```") {
            // Tolerant pairing — see `has_mermaid_fence`: info-carrying
            // lines always open (implicitly closing the previous block, at
            // THIS line's start so the opener is never swallowed); only a
            // bare fence closes, and its own line is part of the range.
            let info = rest.trim();
            if in_fence {
                let source = &text[source_start..pos];
                if is_mermaid || (untagged && looks_like_mermaid_source(source)) {
                    fences.push(MermaidFence {
                        start: block_start,
                        end: if info.is_empty() { line_end } else { pos },
                        source: source.to_string(),
                    });
                }
            }
            block_start = pos;
            source_start = line_end;
            is_mermaid = info.eq_ignore_ascii_case("mermaid");
            untagged = info.is_empty();
            in_fence = true;
        }
        pos = line_end;
    }
    fences
}

/// Canonical correction rules for Mermaid diagrams shared across the codebase.
pub(crate) fn unified_mermaid_rules() -> &'static str {
    "Correction rules:\n\
     1. Syntax & Notes: Fix the exact token or line reported by the renderer. For sequence diagrams, \
     'Note over A,B:' supports at most two participants spanning the range — do not list three or more comma-separated actors. \
     Do not use backticks or HTML tags in labels (only '<br/>' is allowed for line breaks).\n\
     2. Mobile Layout & Aspect Ratio: Always use top-down vertical layouts ('flowchart TD' or 'direction TB'). \
     Never use wide 'LR' layouts or unconstrained horizontal subgraphs that shrink illegibly on mobile screens."
}

/// Format a unified Mermaid syntax error message with context and diagnostic errors.
pub(crate) fn format_mermaid_error(context: &str, errors: &[String]) -> String {
    let quoted = errors.join("\n");
    let rules = unified_mermaid_rules();
    if context.is_empty() {
        format!(
            "Mermaid diagram syntax error.\n\n\
             Renderer diagnostic:\n{quoted}\n\n\
             {rules}"
        )
    } else {
        format!(
            "Mermaid diagram syntax error in {context}.\n\n\
             Renderer diagnostic:\n{quoted}\n\n\
             {rules}"
        )
    }
}

// ── Render-probe seam (dependency inversion) ──
//
// Validation is only meaningful where a renderer exists: the verdict comes
// from the renderer, not from a local parser. The probe resolves in two steps —
// a probe a channel installed at runtime wins, otherwise the channel compiled
// into this build answers, so a process that never constructs a channel (a
// one-shot `run`, a test binary) still validates. Every caller then asks one
// question — "what does the renderer say is wrong with the mermaid in this
// text?" — and gets an empty list when there is nothing to fix (no rendering
// channel in the build, or a clean parse).

/// A channel that can render diagrams. The renderer — not a local parser —
/// produces the verdict, so validation is only meaningful where a renderer
/// exists. Empty return = nothing to fix (no renderer, or clean parse).
pub trait MermaidProbe: Send + Sync {
    fn validate<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Vec<String>>;
}

static PROBE: OnceLock<Box<dyn MermaidProbe>> = OnceLock::new();

/// Installed once by a channel at init; first install wins. Public seam, so a
/// build with no rendering channel compiled in still compiles it — its
/// callers (`validate`) then stay a documented no-op (#326).
pub fn install_probe(p: Box<dyn MermaidProbe>) {
    let _ = PROBE.set(p);
}

/// The installed render probe, if a channel has installed one.
pub fn probe() -> Option<&'static dyn MermaidProbe> {
    PROBE.get().map(|b| b.as_ref())
}

/// The probe a build compiles in, if any: a rendering channel present at
/// compile time answers validation in EVERY process — including one that never
/// constructs that channel (a one-shot `run`, a test binary). A build with no
/// rendering channel has none, and the seam stays the documented no-op.
fn compiled_in_probe() -> Option<&'static dyn MermaidProbe> {
    #[cfg(feature = "telegram")]
    {
        Some(crate::channels::telegram::agent::telegram_probe())
    }
    #[cfg(not(feature = "telegram"))]
    {
        None
    }
}

/// Channel-agnostic entry point: what the renderer reports as wrong with the
/// mermaid in `text`. Empty = nothing to fix.
///
/// A probe installed at runtime wins; otherwise the compiled-in channel
/// answers. Either way the verdict comes from a renderer — this never falls
/// back to a local parser.
pub async fn validate(text: &str) -> Vec<String> {
    validate_with(probe().or_else(compiled_in_probe), text).await
}

/// Probe-explicit core, so both the installed and no-probe cases are testable
/// WITHOUT mutating global state (no test-order dependence).
pub(crate) async fn validate_with(p: Option<&dyn MermaidProbe>, text: &str) -> Vec<String> {
    match p {
        Some(p) => p.validate(text).await,
        None => Vec::new(),
    }
}
