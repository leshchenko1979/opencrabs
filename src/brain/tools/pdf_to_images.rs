//! PDF-to-Images Tool
//!
//! Renders PDF pages to PNG files so the agent can *see* figures,
//! screenshots, diagrams, charts, or scanned content that plain text
//! extraction (`parse_document`) silently drops. Returns the rendered
//! page-image paths; the agent then views them ONE AT A TIME with
//! `analyze_image`, exactly like the auto-ingest path for scanned PDFs.
//!
//! Why a separate tool: a PDF with plenty of text but embedded images
//! takes the text-extraction path on ingest, so the images never reach
//! a vision model. This tool is the on-demand escape hatch for that case.

use super::error::{Result, ToolError};
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;

/// Upper bound on pages rendered in a single call, to cap memory and
/// disk. Mirrors the auto-ingest cap in `utils::file_extract`.
const MAX_PAGES: usize = 100;

pub struct PdfToImagesTool;

#[derive(Debug, Deserialize)]
struct PdfToImagesInput {
    /// Path to the PDF file.
    path: String,

    /// Optional: specific pages to render (1-indexed).
    #[serde(default)]
    pages: Option<Vec<usize>>,

    /// Optional: page range string like "1-5", "3,7,10-12".
    /// Merged with `pages`. Omit to render from page 1 up to the cap.
    #[serde(default)]
    page_range: Option<String>,

    /// Optional: render resolution in DPI (72–300, default 200).
    ///
    /// The renderer additionally caps each page's pixel area, so a
    /// large-format page (a zoning map, an A0 drawing) is rendered at a
    /// lower effective resolution than requested rather than taking the
    /// whole daemon down with it (#814). The output says which
    /// resolution was actually used.
    #[serde(default)]
    dpi: Option<u32>,
}

#[async_trait]
impl Tool for PdfToImagesTool {
    fn name(&self) -> &str {
        "pdf_to_images"
    }

    fn description(&self) -> &str {
        "Render PDF pages to PNG images so you can SEE them with vision. \
         Use this when a PDF contains figures, diagrams, screenshots, charts, \
         tables-as-images, signatures, or scanned content that `parse_document` \
         (text only) cannot convey — e.g. you parsed a PDF's text but the user \
         is asking about something visual in it. Pass `page_range` (e.g. \"3-5\") \
         to render only the pages you need. Returns a list of page-image paths; \
         then call `analyze_image(image='<path>', question='...')` ONE PAGE AT A \
         TIME to view each (do not bundle pages — providers cap request size)."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the PDF file"
                },
                "pages": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 1},
                    "description": "Optional: specific page numbers to render (1-indexed). Prefer `page_range` for spans."
                },
                "page_range": {
                    "type": "string",
                    "description": "Optional: page range like \"1-5\", \"3,7,10-12\". Merged with `pages`. Omit to render from page 1 up to the cap."
                },
                "dpi": {
                    "type": "integer",
                    "minimum": 72,
                    "maximum": 300,
                    "description": "Optional: render resolution in DPI (default 200). Raise it for small text on a large sheet; the renderer still caps each page's pixel area, so a large-format page may come out at a lower effective resolution than requested."
                }
            },
            "required": ["path"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::ReadFiles]
    }

    fn requires_approval(&self) -> bool {
        false
    }

    fn validate_input(&self, input: &Value) -> Result<()> {
        let _: PdfToImagesInput = serde_json::from_value(input.clone())
            .map_err(|e| ToolError::InvalidInput(format!("Invalid input: {}", e)))?;
        Ok(())
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let input: PdfToImagesInput = serde_json::from_value(input)?;

        let path = super::error::resolve_tool_path(&input.path, &context.working_dir());
        if !path.exists() {
            return Ok(ToolResult::error(format!(
                "File not found: {}",
                path.display()
            )));
        }
        if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            != Some("pdf".to_string())
        {
            return Ok(ToolResult::error(
                "pdf_to_images only renders .pdf files. For other formats use parse_document."
                    .to_string(),
            ));
        }

        // Merge `pages` + `page_range` into a sorted, deduped 1-indexed list.
        let mut requested: Vec<usize> = input.pages.clone().unwrap_or_default();
        if let Some(ref spec) = input.page_range {
            requested.extend(super::doc_parser::parse_page_range(spec));
        }
        requested.sort_unstable();
        requested.dedup();

        // The renderer always renders pages 1..=N from the front, returning
        // paths in page order. To honour a range we render up to the highest
        // requested page (capped), then pick the requested ones by index —
        // robust across the pdfium/pdftoppm filename schemes.
        let render_upto = match requested.last() {
            Some(&hi) => hi.min(MAX_PAGES),
            None => MAX_PAGES,
        };

        let requested_dpi = input
            .dpi
            .unwrap_or(crate::utils::pdf_vision::DEFAULT_DPI)
            .clamp(
                crate::utils::pdf_vision::MIN_DPI,
                crate::utils::pdf_vision::MAX_DPI,
            );

        let path_owned = path.clone();
        let out_owned = render_output_dir(&path);
        // The geometry probe and the render both shell out, so they share
        // one blocking hop — `pdfinfo` on the async executor would stall
        // every other task on the worker for the duration of a subprocess.
        // The DPI is resolved here rather than reported afterwards so the
        // answer can say what actually happened: a silently downscaled map
        // is worse than a stated one, because the vision model reads mush
        // and the agent relays it as detail.
        let rendered = tokio::task::spawn_blocking(move || {
            let effective_dpi = crate::utils::pdf_vision::max_page_area_pt(&path_owned, render_upto)
                .map(|area| crate::utils::pdf_vision::effective_dpi(area, requested_dpi))
                .unwrap_or(requested_dpi);
            let pages = crate::utils::pdf_vision::render_pdf_pages(
                path_owned.to_str().unwrap_or(""),
                render_upto,
                out_owned.to_str().unwrap_or(""),
                requested_dpi,
            )?;
            Ok::<_, String>((pages, effective_dpi))
        })
        .await
        .map_err(|e| ToolError::Execution(format!("PDF render task failed: {e}")))?;

        let (all_pages, effective_dpi) = match rendered {
            Ok((p, dpi)) if !p.is_empty() => (p, dpi),
            Ok(_) => {
                return Ok(ToolResult::error(format!(
                    "PDF renderer produced no pages for {}",
                    path.display()
                )));
            }
            Err(e) => {
                return Ok(ToolResult::error(format!(
                    "Failed to render PDF pages: {e}. Install poppler-utils (pdftoppm) or enable the 'pdfium' feature."
                )));
            }
        };


        // Select the requested pages by 1-indexed position; an empty
        // request means "all rendered pages". Out-of-range numbers are
        // reported so the model knows the document was shorter.
        let total_rendered = all_pages.len();
        let mut selected: Vec<(usize, String)> = Vec::new();
        let mut missing: Vec<usize> = Vec::new();
        if requested.is_empty() {
            for (i, p) in all_pages.iter().enumerate() {
                selected.push((i + 1, p.to_string_lossy().to_string()));
            }
        } else {
            for page in requested {
                match all_pages.get(page - 1) {
                    Some(p) => selected.push((page, p.to_string_lossy().to_string())),
                    None => missing.push(page),
                }
            }
        }

        if selected.is_empty() {
            return Ok(ToolResult::error(format!(
                "None of the requested pages exist (document rendered {total_rendered} page(s))."
            )));
        }

        let path_list: String = selected
            .iter()
            .map(|(n, p)| format!("- Page {n}: {p}"))
            .collect::<Vec<_>>()
            .join("\n");

        let mut output = format!(
            "Rendered {} page(s) of {} as images. Call `analyze_image(image='<path>', \
             question='...')` ONE PAGE AT A TIME to view each — do NOT bundle pages, \
             providers cap request body size.\n{path_list}",
            selected.len(),
            path.display(),
        );
        if !missing.is_empty() {
            output.push_str(&format!(
                "\n[Requested pages not present (document has {total_rendered} page(s)): {missing:?}]"
            ));
        }
        // Only stated when the cap actually bit. A note on every render
        // would be noise; silence about a downscale is a trap.
        if effective_dpi < requested_dpi {
            output.push_str(&format!(
                "\n[Rendered at {effective_dpi} DPI instead of {requested_dpi}: the largest page \
                 would exceed the per-page pixel budget, so it was downscaled rather than \
                 OOM-killing the daemon. Small text may be unreadable; raise `dpi` only for a \
                 document whose pages are smaller than this one's.]"
            ));
        }

        Ok(ToolResult::success(output)
            .with_metadata("path".to_string(), path.display().to_string())
            .with_metadata("pages_rendered".to_string(), selected.len().to_string())
            .with_metadata("requested_dpi".to_string(), requested_dpi.to_string())
            .with_metadata("effective_dpi".to_string(), effective_dpi.to_string()))
    }
}

/// Directory to render page PNGs into: a sibling `<stem>_pages` folder
/// next to the PDF, keeping renders grouped and easy to clean up.
fn render_output_dir(pdf: &Path) -> std::path::PathBuf {
    let stem = pdf
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("document");
    let parent = pdf.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}_pages"))
}
