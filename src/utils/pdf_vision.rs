//! PDF page rendering to PNG images.
//!
//! Renders individual PDF pages as PNG files using (in order of preference):
//! 1. `pdfium-render` crate, bound to the system Pdfium library (feature
//!    `pdfium`, on by default). Nothing is bundled: `bind_to_system_library`
//!    needs libpdfium present, and returns an error when it is missing, which
//!    is what falls through to the next strategy.
//! 2. Shell fallback to `pdftoppm` (poppler-utils)
//!
//! Pages are processed in configurable batches to cap memory usage.
//! On failure, any partial renders are cleaned up automatically.
//!
//! A page COUNT is the wrong quantity to cap: raster memory grows with
//! `page area x dpi^2`, so ten A4 pages and ten A0 pages differ by ~100x
//! in what they allocate. Every render is therefore bounded by
//! [`MAX_PIXELS_PER_PAGE`] against the document's own page geometry, and
//! the caller can trade resolution for legibility through `dpi` (#814).

use std::fs;
use std::path::{Path, PathBuf};

/// Number of pages processed per batch to limit memory.
const BATCH_SIZE: usize = 10;

/// Default render resolution. 200 DPI keeps A4 body text legible to a
/// vision model, which is why it survived as a constant for so long —
/// the defect in #814 was never the value, it was that it was the ONLY
/// value a caller could get.
pub const DEFAULT_DPI: u32 = 200;

/// Lowest resolution worth rendering at. Below this, body text turns to
/// mush and the vision model describes glyph soup — worse than an honest
/// refusal, because the caller cannot tell the two apart.
pub const MIN_DPI: u32 = 72;

/// Highest resolution a caller may request. Past ~300 DPI the raster
/// costs memory and vision tokens without adding detail any provider
/// still has after its own downscale.
pub const MAX_DPI: u32 = 300;

/// Pixel budget for ONE rendered page.
///
/// This — not the page count — is the quantity that decides whether a
/// render survives. Sized from the #814 incident: a single `pdftoppm`
/// reached 946 MB anon-rss inside the daemon's 1 GiB cgroup and took the
/// whole service down four times in 117 s. That works out at 15–30 bytes
/// per output pixel depending on the page's true dimensions (A0 at
/// 200 DPI is ~62 Mpx, A1 ~31 Mpx), so 10 Mpx costs ~150–300 MB — a
/// third of the cgroup at worst, leaving room for the lanes sharing it.
/// A4 at 200 DPI is 3.9 Mpx, so ordinary documents are untouched and
/// only large-format pages lose resolution.
pub(crate) const MAX_PIXELS_PER_PAGE: f64 = 10_000_000.0;

/// Target pixel WIDTH the pdfium path renders every page at. It bounds an
/// A0 sheet to ~5.7 Mpx, which is why that path needs no `dpi` argument.
const PDFIUM_TARGET_WIDTH: u32 = 2000;

/// Side of the pixel box that bounds one page, for `pdftoppm -scale-to`
/// (which fits each page inside an N x N square, aspect preserved).
pub(crate) fn pixel_box() -> u32 {
    MAX_PIXELS_PER_PAGE.sqrt().floor() as u32
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// How the rasteriser bounded a render.
///
/// Carried out of the renderer rather than recomputed by the caller.
/// Recomputing it meant a SECOND `pdfinfo` probe and a second copy of the
/// same decision — which then disagreed with the renderer on the one branch
/// that matters (unmeasurable geometry), so the tool told the model
/// "200 DPI" while `pdftoppm` had rendered a bounded pixel box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderBound {
    /// Rendered at this fixed resolution (`pdftoppm -r`).
    Dpi(u32),
    /// Fitted inside an NxN pixel box (`pdftoppm -scale-to`): no single DPI
    /// describes the output, and the effective resolution varies per page.
    PixelBox(u32),
    /// Bounded by the pdfium target width in pixels. That path applies the
    /// same width to every page, so it is not a response to page size.
    TargetWidth(u32),
}

/// What a render actually produced, and how it was bounded.
///
/// `Debug` is not decoration: `Result::expect_err` in the tests requires
/// the Ok type to be printable.
#[derive(Debug)]
pub struct RenderedPages {
    /// Rendered PNG paths, in page order.
    pub pages: Vec<PathBuf>,
    /// How the raster was bounded — the caller reports THIS, never the
    /// resolution it requested.
    pub bound: RenderBound,
}

/// Render up to `max_pages` pages of `pdf_path` as PNG files in `output_dir`.
///
/// - Creates `output_dir` if it does not exist.
/// - Pages beyond `max_pages` are skipped (a warning is logged).
/// - `dpi` is the requested resolution, clamped to
///   [`MIN_DPI`]..=[`MAX_DPI`] and then reduced further when the page is
///   large-format, so that no single page's raster exceeds
///   [`MAX_PIXELS_PER_PAGE`]. It applies to the `pdftoppm` path; the
///   pdfium path is already bounded by a target width.
/// - Returns the rendered pages together with [`RenderBound`]: the fixed
///   DPI used, or the pixel box / target width that bounded the raster
///   instead. Reporting that distinction is the whole point — a caller
///   that assumes it got the DPI it asked for will read a downscaled
///   map's mush as detail and relay it onward as such.
/// - On failure any partially-rendered files are removed.
pub fn render_pdf_pages(
    pdf_path: &str,
    max_pages: usize,
    output_dir: &str,
    dpi: u32,
) -> Result<RenderedPages, String> {
    let pdf = PathBuf::from(pdf_path);
    let out = PathBuf::from(output_dir);

    if !pdf.exists() {
        return Err(format!("PDF file not found: {}", pdf_path));
    }

    // Ensure output directory exists.
    fs::create_dir_all(&out)
        .map_err(|e| format!("Failed to create output directory '{}': {}", output_dir, e))?;

    let result = render_pdf_pages_inner(&pdf, max_pages, &out, dpi);

    // On total failure (no pages rendered at all), clean up any stray
    // partial renders from earlier strategies. A partial success
    // (Ok with some pages) is preserved by `render_pdf_pages_inner`
    // — losing the good pages because the last batch failed was the
    // bug that surfaced as "PDFs truncated after page 5".
    if let Err(ref err) = result {
        tracing::warn!("PDF render failed, cleaning up partial output: {}", err);
        cleanup_dir(&out);
    }

    result
}

// ---------------------------------------------------------------------------
// Dispatch: pdfium-render → pdftoppm → error
// ---------------------------------------------------------------------------

fn render_pdf_pages_inner(
    pdf_path: &Path,
    max_pages: usize,
    output_dir: &Path,
    dpi: u32,
) -> Result<RenderedPages, String> {
    // Strategy 1: pdfium-render crate. It bounds every page by a target
    // pixel WIDTH, so `dpi` is not needed here and no fixed DPI describes
    // the result; see `render_with_pdfium`.
    match render_with_pdfium(pdf_path, max_pages, output_dir) {
        Ok(pages) => {
            return Ok(RenderedPages {
                pages,
                bound: RenderBound::TargetWidth(PDFIUM_TARGET_WIDTH),
            });
        }
        Err(e) => tracing::debug!("pdfium-render unavailable or failed: {}", e),
    }

    // Strategy 2: shell pdftoppm fallback. It reports how it bounded the
    // raster: `-r <dpi>` is a fixed resolution, `-scale-to` is not.
    match render_with_pdftoppm(pdf_path, max_pages, output_dir, dpi) {
        Ok((pages, bound)) => return Ok(RenderedPages { pages, bound }),
        Err(e) => tracing::debug!("pdftoppm unavailable or failed: {}", e),
    }

    Err(
        "No PDF renderer available. Install poppler — macOS: `brew install poppler`, \
         Debian/Ubuntu: `apt install poppler-utils`, Fedora: `dnf install poppler-utils` \
         (or build with the optional `pdfium` feature)."
            .into(),
    )
}

/// Page geometry of a PDF, as far as `pdfinfo` can report it.
///
/// Both fields are optional because `pdfinfo` degrades in steps: absent
/// entirely (not installed), present but unable to open the file, or
/// present and reporting a page count while saying nothing usable about
/// page size. Callers must treat "unknown" as "unbounded", never as
/// "small" — that assumption is what let #814 through.
#[derive(Debug, Default)]
struct PdfGeometry {
    /// Total pages in the document.
    pages: Option<usize>,
    /// Area of the largest page, in square points (1/72 inch). Raster
    /// memory is proportional to this times `dpi^2`.
    max_area_pt: Option<f64>,
}

/// Ask `pdfinfo` for the page count and the per-page sizes, inspecting
/// only the first `limit` pages — those are the only ones a render will
/// touch, and `-f`/`-l` keeps a 5000-page document from printing 10k
/// lines of geometry.
fn pdf_geometry(pdf_path: &Path, limit: usize) -> PdfGeometry {
    let mut geo = PdfGeometry::default();
    let Some(path_str) = pdf_path.to_str() else {
        return geo;
    };
    let Ok(out) = std::process::Command::new("pdfinfo")
        .args(["-f", "1", "-l", &limit.max(1).to_string(), path_str])
        .output()
    else {
        return geo;
    };
    if !out.status.success() {
        return geo;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(rest) = line.strip_prefix("Pages:") {
            geo.pages = rest.trim().parse::<usize>().ok();
        } else if let Some((w, h)) = parse_page_size_line(line) {
            let area = w * h;
            geo.max_area_pt = Some(match geo.max_area_pt {
                Some(prev) if prev > area => prev,
                _ => area,
            });
        }
    }
    geo
}

/// Parse one `pdfinfo` per-page line into its width and height in points.
///
/// The format is `Page    1 size:  595 x 842 pts (A4)` — note the
/// document-level `Page size:  595 x 842 pts (A4)` line, which differs
/// only by the missing page number. Requiring the number to parse is
/// what tells the two apart, so the document-level line never
/// contributes a bogus "page 0".
pub(crate) fn parse_page_size_line(line: &str) -> Option<(f64, f64)> {
    let rest = line.strip_prefix("Page")?;
    let (num, tail) = rest.split_once(" size:")?;
    num.trim().parse::<usize>().ok()?;
    let dims = tail.trim().split(" pts").next()?.trim();
    let (w, h) = dims.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Resolution to actually render at, given the largest page's area.
///
/// `requested` is honoured for ordinary documents and scaled down for
/// large-format pages until the raster fits [`MAX_PIXELS_PER_PAGE`]. It
/// never drops below [`MIN_DPI`]: past that point resolution is no
/// longer the lever, and the caller gets a bounded pixel box instead
/// (see `scale_to_box`).
pub fn effective_dpi(max_area_pt: f64, requested: u32) -> u32 {
    let requested = requested.clamp(MIN_DPI, MAX_DPI);
    // Names NaN explicitly rather than relying on `!(x > 0.0)` being true
    // for it: the comparison below is false for NaN, so an unguarded NaN
    // would fall through and silently disable the budget — the exact
    // shape of #814.
    if max_area_pt.is_nan() || max_area_pt <= 0.0 {
        return requested;
    }
    let px = max_area_pt * (requested as f64 / 72.0).powi(2);
    if px <= MAX_PIXELS_PER_PAGE {
        return requested;
    }
    // px = area * (dpi/72)^2  =>  dpi = 72 * sqrt(px / area)
    let fitted = 72.0 * (MAX_PIXELS_PER_PAGE / max_area_pt).sqrt();
    (fitted.floor() as u32).clamp(MIN_DPI, requested)
}

/// `pdftoppm -scale-to` box needed when even [`MIN_DPI`] overflows the
/// budget — a page so large that no resolution is safe.
///
/// Returns `None` when `-r dpi` already stays inside the budget, because
/// `-scale-to` OVERRIDES `-r` in poppler (it re-derives the resolution
/// from the box), so passing both would silently discard the caller's
/// requested resolution and upscale small pages.
pub(crate) fn scale_to_box(max_area_pt: f64, dpi: u32) -> Option<u32> {
    if max_area_pt.is_nan() || max_area_pt <= 0.0 {
        return None;
    }
    let px = max_area_pt * (dpi as f64 / 72.0).powi(2);
    if px <= MAX_PIXELS_PER_PAGE {
        return None;
    }
    Some(pixel_box())
}

// ---------------------------------------------------------------------------
// pdfium-render implementation
// ---------------------------------------------------------------------------

/// Process-wide pdfium handle. `pdfium-render` 0.9.1 keeps its loaded
/// library in a process-global `OnceCell` that is never reset: a second
/// bind attempt in the same process returns
/// `PdfiumLibraryBindingsAlreadyInitialized`, and two concurrent attempts
/// race through the crate's guard-less `is_none()` check into
/// `Pdfium::new`'s `assert!(BINDINGS.get().is_none())`, panicking the
/// loser (#1715). Binding exactly once behind
/// `once_cell::sync::OnceCell::get_or_try_init` serialises the bind and
/// lets every later render reuse the handle; a *failed* bind is not
/// cached, so hosts without libpdfium still fall through to the pdftoppm
/// strategy on every call instead of latching the first failure. The `std`
/// `OnceLock::get_or_try_init` is still unstable on this toolchain, hence
/// the `once_cell` crate, matching the `memory/embedding.rs` precedent.
///
/// `Pdfium::default()` is not a substitute: it re-runs the same guard-less
/// racy bind, and per its own docs it "will panic if no suitable Pdfium
/// library can be loaded", which inside `spawn_blocking` surfaces as a
/// `JoinError` with no fallback at all (#1716).
#[cfg(feature = "pdfium")]
static PDFIUM: once_cell::sync::OnceCell<pdfium_render::prelude::Pdfium> =
    once_cell::sync::OnceCell::new();

/// Bind pdfium once per process; see [`PDFIUM`] for why re-binding is a
/// guaranteed failure and concurrent re-binding a panic landmine. The
/// error is already the caller-facing string so no remapping is needed.
#[cfg(feature = "pdfium")]
pub(crate) fn shared_pdfium() -> Result<&'static pdfium_render::prelude::Pdfium, String> {
    PDFIUM
        .get_or_try_init(|| {
            pdfium_render::prelude::Pdfium::bind_to_system_library()
                .map(pdfium_render::prelude::Pdfium::new)
        })
        .map_err(|e| format!("Cannot bind pdfium library: {}", e))
}

#[cfg(feature = "pdfium")]
fn render_with_pdfium(
    pdf_path: &Path,
    max_pages: usize,
    output_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    use pdfium_render::prelude::*;

    let pdfium = shared_pdfium()?;

    let document = pdfium
        .load_pdf_from_file(pdf_path, None)
        .map_err(|e| format!("Failed to open PDF '{}': {}", pdf_path.display(), e))?;

    let total_pages = document.pages().len() as usize;
    let pages_to_render = total_pages.min(max_pages);

    if total_pages > max_pages {
        tracing::warn!(
            "PDF has {} pages but max_pages={}; skipping pages {}–{}",
            total_pages,
            max_pages,
            max_pages + 1,
            total_pages,
        );
    }

    let mut rendered = Vec::with_capacity(pages_to_render);
    let mut page_idx: usize = 0;

    while page_idx < pages_to_render {
        let batch_end = (page_idx + BATCH_SIZE).min(pages_to_render);

        for i in page_idx..batch_end {
            let page = document
                .pages()
                .get(i as PdfPageIndex)
                .map_err(|e| format!("Failed to get page {}: {}", i + 1, e))?;

            // Bounded by target WIDTH, which is why this path needs no
            // `dpi` argument: 2000 px across is 5.7 Mpx even on an A0
            // page, so it cannot reproduce the #814 memory blowup.
            let bitmap = page
                .render_with_config(
                    &PdfRenderConfig::new().set_target_width(PDFIUM_TARGET_WIDTH as i32),
                )
                .map_err(|e| format!("Failed to render page {}: {}", i + 1, e))?;

            let file_name = format!("page_{:04}.png", i + 1);
            let file_path = output_dir.join(&file_name);

            bitmap
                .as_image()
                .map_err(|e| format!("Failed to convert page {} to image: {}", i + 1, e))?
                .save(&file_path)
                .map_err(|e| format!("Failed to save page {}: {}", i + 1, e))?;

            rendered.push(file_path);
        }

        page_idx = batch_end;
        tracing::debug!("Rendered batch: pages {}/{}", page_idx, pages_to_render,);
    }

    Ok(rendered)
}

#[cfg(not(feature = "pdfium"))]
fn render_with_pdfium(
    _pdf_path: &Path,
    _max_pages: usize,
    _output_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    Err("pdfium feature not enabled".into())
}

// ---------------------------------------------------------------------------
// pdftoppm shell fallback
// ---------------------------------------------------------------------------

fn render_with_pdftoppm(
    pdf_path: &Path,
    max_pages: usize,
    output_dir: &Path,
    requested_dpi: u32,
) -> Result<(Vec<PathBuf>, RenderBound), String> {
    // Check that pdftoppm is available.
    which::which("pdftoppm").map_err(|_| "pdftoppm not found in PATH".to_string())?;

    let pdf_path_str = pdf_path
        .to_str()
        .ok_or_else(|| "PDF path is not valid UTF-8".to_string())?;
    let out_dir_str = output_dir
        .to_str()
        .ok_or_else(|| "Output directory path is not valid UTF-8".to_string())?;

    let geometry = pdf_geometry(pdf_path, max_pages);

    // Bound the render loop to the document's actual page count when
    // pdfinfo is available. Without this, batches past EOF make
    // pdftoppm error with "Wrong page range given: the first page (N)
    // can not be after the last page (M)" — the whole render then
    // returns Err and the caller wipes the already-rendered batches.
    // When pdfinfo isn't installed we fall back to max_pages and rely
    // on per-batch error tolerance below.
    let effective_max = match geometry.pages {
        Some(total) => total.min(max_pages),
        None => max_pages,
    };
    if effective_max == 0 {
        return Err("pdftoppm: PDF reports zero pages".into());
    }

    // Resolve the resolution against the page's own area. `-r 200` used
    // to be the only value obtainable, which is comfortable for an A4
    // text page (3.9 Mpx) and fatal for an A0 map (62 Mpx) — one such
    // page took the daemon down four times in 117 s (#814).
    //
    // Two bounds, mutually exclusive:
    // 1. `-r` at the highest resolution the page area allows, so normal
    //    documents keep the caller's requested DPI.
    // 2. `-scale-to`, when even MIN_DPI overflows the budget or pdfinfo
    //    could not measure the page. It fits each page inside a fixed
    //    pixel box, so the raster stays bounded whatever the page size —
    //    the only guarantee that survives unknown geometry. Never passed
    //    beside `-r`: poppler re-derives the resolution from the box, so
    //    `-r` would be silently discarded and small pages upscaled.
    let dpi = match geometry.max_area_pt {
        Some(area) => effective_dpi(area, requested_dpi),
        None => requested_dpi.clamp(MIN_DPI, MAX_DPI),
    };
    let scale_to = match geometry.max_area_pt {
        Some(area) => scale_to_box(area, dpi),
        None => Some(pixel_box()),
    };
    // The bound is decided HERE and travels back to the caller, so nothing
    // downstream has to re-derive it (and cannot disagree with what ran).
    let bound = match scale_to {
        Some(box_px) => RenderBound::PixelBox(box_px),
        None => RenderBound::Dpi(dpi),
    };
    if dpi < requested_dpi || scale_to.is_some() {
        tracing::warn!(
            "pdftoppm: large-format page — rendering at {} DPI{} (requested {} DPI) to stay \
             inside the {} Mpx per-page budget (#814)",
            dpi,
            match scale_to {
                Some(box_px) => format!(" in a {box_px}x{box_px} px box"),
                None => String::new(),
            },
            requested_dpi,
            (MAX_PIXELS_PER_PAGE / 1_000_000.0) as u32,
        );
    }

    // pdftoppm generates files named <prefix>-01.png, <prefix>-02.png, …
    let prefix = "page";
    let mut rendered: Vec<PathBuf> = Vec::with_capacity(effective_max);

    // Process in batches. We tolerate per-batch errors: a partial render
    // (early pages succeed, a late batch fails) is more useful than no
    // pages at all. Only return Err if every batch fails.
    let mut start = 1;
    let mut last_err: Option<String> = None;
    while start <= effective_max {
        let batch_end = (start + BATCH_SIZE - 1).min(effective_max);

        let mut cmd = std::process::Command::new("pdftoppm");
        cmd.args([
            "-png",
            "-f",
            &start.to_string(),
            "-l",
            &batch_end.to_string(),
        ]);
        match scale_to {
            // Mutually exclusive by construction: poppler re-derives the
            // resolution from the box, so a `-r` beside `-scale-to` is
            // silently discarded — and worse, small pages would be
            // upscaled to the box while large ones shrank.
            Some(box_px) => cmd.args(["-scale-to", &box_px.to_string()]),
            None => cmd.args(["-r", &dpi.to_string()]),
        };
        let output = cmd
            .arg(pdf_path_str)
            .arg(format!("{}/{}", out_dir_str, prefix))
            .output()
            .map_err(|e| format!("Failed to execute pdftoppm: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                "pdftoppm batch {}-{} failed: {} — keeping earlier pages",
                start,
                batch_end,
                stderr.trim()
            );
            last_err = Some(stderr.trim().to_string());
            // Don't return — try the next batch. PDFs with corrupt
            // pages in the middle still surface their good pages.
            start = batch_end + 1;
            continue;
        }

        // Collect the generated files for this batch. pdftoppm pads page
        // numbers to the digit-width of the *last page number rendered*,
        // not of the document: `-f 1 -l 3` writes `page-1.png`, while
        // `-f 1 -l 99` writes `page-01.png`. Every batch ending at a
        // single-digit page therefore produced files none of the old
        // 2-to-4 widths could find, and the pages were silently dropped
        // (#1715). Try every candidate width instead of guessing the one.
        for page_num in start..=batch_end {
            if let Some(p) = pdftoppm_output_names(prefix, page_num)
                .into_iter()
                .find_map(|name| output_dir.join(name).canonicalize().ok())
            {
                rendered.push(p);
            }
        }

        start = batch_end + 1;
    }

    if rendered.is_empty() {
        return Err(format!(
            "pdftoppm produced no output files{}",
            last_err
                .map(|e| format!(" (last error: {e})"))
                .unwrap_or_default()
        ));
    }

    Ok((rendered, bound))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Filenames `pdftoppm` may have written for `page_num`: the digits
/// left-padded to every width from 1 to 4. Width 1 is the unpadded form
/// `pdftoppm` uses whenever the rendered range ends at page 9 or below
/// (`-f 1 -l 3` writes `page-1.png`), which the previous fixed 2-to-4
/// lookup never tried (#1715).
pub(crate) fn pdftoppm_output_names(prefix: &str, page_num: usize) -> Vec<String> {
    (1..=4)
        .map(|w| format!("{}-{:0w$}.png", prefix, page_num, w = w))
        .collect()
}

/// Remove all files inside `dir` (but not the directory itself).
fn cleanup_dir(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            let _ = fs::remove_file(entry.path());
        }
    }
}
