//! Tests for `utils::pdf_vision::render_pdf_pages`.
//!
//! Covers the regression where `pdftoppm` would loop past the document's
//! actual page count, fail on a later batch, and cause the whole render
//! to be discarded (the "truncated after page 5" symptom). We can't
//! unit-test the pdftoppm shell behavior directly without a real PDF
//! and the binary on PATH, so these focus on what we can deterministically
//! check: the function entry-point contract.

use crate::utils::pdf_vision::{
    effective_dpi, parse_page_size_line, pixel_box, render_pdf_pages, scale_to_box, DEFAULT_DPI,
    MAX_DPI, MAX_PIXELS_PER_PAGE, MIN_DPI,
};
use std::fs;

#[test]
fn missing_pdf_returns_error_with_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out_dir = tmp.path().join("pages");
    let result = render_pdf_pages("/no/such/file.pdf", 10, out_dir.to_str().unwrap(), DEFAULT_DPI);
    let err = result.expect_err("missing pdf must error");
    assert!(err.contains("/no/such/file.pdf"));
}

#[test]
fn output_directory_is_created_if_missing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // Write a 1-byte fake "pdf" so existence check passes — render
    // itself will fail (not a real PDF) but we only care that the
    // output directory was created during setup.
    let pdf = tmp.path().join("fake.pdf");
    fs::write(&pdf, b"%PDF-1.4\n").expect("write fake pdf");
    let out = tmp.path().join("nested").join("renders");
    assert!(!out.exists(), "precondition: output dir must not exist yet");
    let _ = render_pdf_pages(pdf.to_str().unwrap(), 5, out.to_str().unwrap(), DEFAULT_DPI);
    assert!(out.exists(), "output dir must be created on entry");
}

/// When no PDF renderer succeeds (pdfium feature off + no valid input
/// for pdftoppm), the helper must surface an actionable error rather
/// than panic or silently produce an empty Ok.
#[test]
fn unrenderable_pdf_returns_err_not_empty_ok() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let pdf = tmp.path().join("not-really-a-pdf.pdf");
    fs::write(&pdf, b"this is not a pdf").expect("write fake");
    let out = tmp.path().join("out");
    let result = render_pdf_pages(pdf.to_str().unwrap(), 100, out.to_str().unwrap(), DEFAULT_DPI);
    // Either an Err (pdftoppm rejected the input) or an Ok with zero
    // pages — both are acceptable contracts; an Ok with no pages would
    // be the bug we explicitly want to avoid.
    if let Ok(paths) = &result {
        assert!(
            !paths.is_empty(),
            "Ok(vec![]) is forbidden — must be Err if no pages rendered"
        );
    }
}

/// #1715 defect 1: pdfium used to bind on every render call, so the
/// second and every later render in the same process failed with
/// `PdfiumLibraryBindingsAlreadyInitialized` (quietly degrading to
/// pdftoppm), and two concurrent renders could race the crate's
/// guard-less rebind into a panic. `shared_pdfium` must hand out the
/// same process-wide handle on every call. Gated on a successful first
/// bind so hosts without libpdfium installed stay green.
#[cfg(feature = "pdfium")]
#[test]
fn pdfium_bind_is_reused_across_calls() {
    use crate::utils::pdf_vision::shared_pdfium;

    let Ok(handle) = shared_pdfium() else {
        return; // no libpdfium on this host - nothing bound yet to reuse
    };
    let again = shared_pdfium().expect("second access must reuse the first bind, not rebind");
    assert!(
        std::ptr::eq(handle, again),
        "second access produced a different Pdfium - a rebind was attempted"
    );
}

/// #1715 defect 2: the pdftoppm output lookup only tried 2-to-4-digit
/// zero-padding, but pdftoppm pads to the digit-width of the last
/// rendered page number, so ranges ending at page 9 or below write
/// unpadded names like `page-1.png`. Those batches rendered fine and
/// then collected zero pages. The candidate set must include the
/// single-digit (unpadded) width.
#[test]
fn pdftoppm_candidates_include_unpadded_width() {
    use crate::utils::pdf_vision::pdftoppm_output_names;

    let names = pdftoppm_output_names("page", 1);
    for expected in ["page-1.png", "page-01.png", "page-001.png", "page-0001.png"] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing candidate {expected}: {names:?}"
        );
    }
    assert_eq!(names.len(), 4, "exactly one candidate per width");
}

// ---------------------------------------------------------------------------
// #814: the render resolution must be bounded by the page's AREA, not by
// the page count. `BATCH_SIZE` counted pages and said nothing about how
// many pixels ten pages hold, so ten A0 maps rendered exactly like ten A4
// letters until `pdftoppm` hit 946 MB anon-rss inside the daemon's 1 GiB
// cgroup and the service was killed four times in 117 s.
// ---------------------------------------------------------------------------

/// A4 at 200 DPI is 3.9 Mpx — comfortably inside the budget, so the
/// caller's request must survive untouched. This is the leg that stops
/// the fix from "protecting" ordinary documents into illegibility.
#[test]
fn effective_dpi_keeps_the_request_for_an_a4_page() {
    let a4_area_pt = 595.0 * 842.0;
    assert_eq!(effective_dpi(a4_area_pt, 200), 200);
    assert_eq!(effective_dpi(a4_area_pt, 300), 300);
}

/// A0 at 200 DPI is ~62 Mpx, six times the budget: the render must come
/// down to the highest resolution that still fits. 80 DPI is the
/// arithmetic answer (72 * sqrt(10e6 / 8.03e6) = 80.3), and asserting
/// the exact value is the point — a clamp that "rounds" to MIN_DPI would
/// throw away four fifths of the available detail for no reason.
#[test]
fn effective_dpi_clamps_an_a0_page() {
    let a0_area_pt = 2384.0 * 3370.0;
    assert_eq!(effective_dpi(a0_area_pt, 200), 80);
}

/// Past MIN_DPI resolution stops being the lever: a page this large
/// cannot be rendered safely at any DPI, so the floor holds and the
/// caller gets a bounded pixel box instead (`scale_to_box`).
#[test]
fn effective_dpi_never_goes_below_the_floor() {
    let enormous = 100_000_000.0;
    assert_eq!(effective_dpi(enormous, 300), MIN_DPI);
}

/// An unmeasurable page must not silently disable the budget. NaN is the
/// value a failed computation actually produces, and `NaN > x` is false,
/// so a naive guard would fall through to the unbounded branch — the
/// exact shape of the #814 regression.
#[test]
fn effective_dpi_treats_unmeasurable_area_as_no_opinion() {
    assert_eq!(effective_dpi(f64::NAN, 200), 200);
    assert_eq!(effective_dpi(0.0, 200), 200);
    assert_eq!(effective_dpi(-5.0, 200), 200);
}

/// A non-finite area must take the BOUNDED path, not the "no opinion"
/// one: infinity means the geometry is enormous, and treating it as
/// unmeasurable would render at the requested DPI with no cap at all —
/// the unbounded arm that #814 died in.
#[test]
fn effective_dpi_bounds_an_infinite_area() {
    assert_eq!(effective_dpi(f64::INFINITY, 300), MIN_DPI);
}

/// The caller's own bounds are enforced here too, so a `dpi: 5000` from
/// a model cannot spend the budget on vision tokens no provider keeps.
#[test]
fn effective_dpi_clamps_the_request_itself() {
    let a4_area_pt = 595.0 * 842.0;
    assert_eq!(effective_dpi(a4_area_pt, 5), MIN_DPI);
    assert_eq!(effective_dpi(a4_area_pt, 5000), MAX_DPI);
}

/// `pdfinfo` prints a document-level `Page size:` line that differs from
/// the per-page one only by the missing page number. Reading it as page
/// 0 would make the largest "page" the document default and clamp every
/// render to a resolution the real pages never needed.
#[test]
fn parse_page_size_line_ignores_the_document_level_line() {
    assert_eq!(
        parse_page_size_line("Page size:       595 x 842 pts (A4)"),
        None
    );
}

/// The per-page line is the one that carries geometry per page.
#[test]
fn parse_page_size_line_reads_a_per_page_line() {
    assert_eq!(
        parse_page_size_line("Page    1 size:  595 x 842 pts (A4)"),
        Some((595.0, 842.0))
    );
    assert_eq!(
        parse_page_size_line("Page   12 size: 2383.94 x 3370.39 pts (A0)"),
        Some((2383.94, 3370.39))
    );
}

/// Anything pdfinfo might print that is not a page size must yield
/// `None` rather than a half-parsed number: a wrong area is worse than
/// an absent one, because absent falls back to the bounded pixel box.
#[test]
fn parse_page_size_line_rejects_non_geometry_lines() {
    for line in [
        "",
        "Pages:           12",
        "Page rot:        0",
        "Page    1 size:  unknown",
    ] {
        assert_eq!(parse_page_size_line(line), None, "line: {line:?}");
    }
}

/// The paper name is decoration; the numbers are the geometry. Dropping
/// the trailing `(A4)` must not change the reading.
#[test]
fn parse_page_size_line_does_not_need_the_paper_name() {
    assert_eq!(
        parse_page_size_line("Page    1 size:  595 x 842 pts"),
        Some((595.0, 842.0))
    );
}

/// The pixel box is the SECOND lever, used only when no fixed resolution
/// fits. Two properties matter: it must actually stay inside the budget
/// (it is `floor(sqrt(budget))`, so the square is just under it), and it
/// must not fire for a page that a fitted DPI already handles — passing
/// `-scale-to` beside `-r` makes poppler discard the resolution and
/// upscale small pages, so "both" is never a valid answer.
#[test]
fn pixel_box_stays_inside_the_budget_and_only_fires_when_no_dpi_fits() {
    let b = pixel_box();
    assert!(
        (b as f64).powi(2) <= MAX_PIXELS_PER_PAGE,
        "box {b}x{b} exceeds the {MAX_PIXELS_PER_PAGE} px budget"
    );
    assert!(
        ((b + 1) as f64).powi(2) > MAX_PIXELS_PER_PAGE,
        "box {b} is smaller than it needs to be"
    );

    // A4 and A0 both fit once the DPI is fitted to the page area, so
    // neither may take the box path.
    for area_pt in [595.0 * 842.0, 2384.0 * 3370.0] {
        let fitted = effective_dpi(area_pt, 200);
        assert_eq!(
            scale_to_box(area_pt, fitted),
            None,
            "area {area_pt} pt^2 fits at {fitted} DPI, so no box is needed"
        );
    }

    // A page so large that even MIN_DPI overflows: resolution has run out,
    // so the box is the only bound left.
    let enormous = 1.0e8;
    let fitted = effective_dpi(enormous, 200);
    assert_eq!(fitted, MIN_DPI, "the floor must hold before the box is used");
    assert_eq!(scale_to_box(enormous, fitted), Some(b));
}
