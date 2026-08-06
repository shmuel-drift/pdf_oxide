//! Redaction overlay content-stream generation (#231, T13 — guarantee
//! G7: an opaque mark is the *only* thing drawn where content was
//! removed).
//!
//! ISO 32000-1:2008 §12.5.6.23 / Table 192 overlay precedence is
//! `RO` form XObject > `OverlayText` (+`DA`,+`Q`,+`Repeat`) > `IC`
//! solid fill > (no `IC`) a default solid fill in destructive mode —
//! "transparent + removed" is visually confusing and risks operator
//! error, so the default is an opaque block.
//!
//! `RO`/`OverlayText` come from the source `/Redact` annotation and are
//! resolved by the annotation layer; this module owns *only* the pure
//! geometry-to-content-stream-bytes step for an already-resolved
//! (region, fill) pair (SRP). It performs no I/O; the engine appends
//! these bytes after the pruned content so the overlay is on top.
//!
//! When the rewritten content stream leaves a non-identity CTM active,
//! page-space region coordinates must be mapped through that CTM's
//! inverse before emission — otherwise the opaque block is painted in
//! the wrong place (glyphs were classified in page space; overlay ops
//! are interpreted in stream space).

use super::classify::transform_bbox;
use super::image_prune::invert_affine;
use super::options::RedactionOptions;
use super::region::RedactionRegion;
use crate::content::graphics_state::Matrix;
use crate::geometry::Rect;
use std::fmt::Write as _;

/// Format a coordinate as a PDF real: fixed-point, trailing zeros and a
/// dangling `.` trimmed, never scientific notation (PDF has no exponent
/// form — ISO 32000-1 §7.3.3). Non-finite ⇒ `0` (fail safe; the overlay
/// must still draw *something* opaque).
fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0".to_string();
    }
    let mut s = format!("{v:.4}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    if s == "-0" {
        s = "0".to_string();
    }
    s
}

/// Resolve the overlay fill colour for a region per the precedence
/// above: explicit region `fill` (the `/IC`) wins; otherwise the
/// configured default *iff* `draw_overlay_when_no_ic`. `None` ⇒ draw no
/// overlay (caller still removed the content; the area is just blank).
fn resolved_fill(region: &RedactionRegion, opts: &RedactionOptions) -> Option<[f32; 3]> {
    match region.fill {
        Some(c) => Some(c),
        None if opts.draw_overlay_when_no_ic => Some(opts.default_fill),
        None => None,
    }
}

/// Map a page-space redaction region into the coordinate space of a
/// content stream whose active CTM is `ctm`.
///
/// Overlay operators are appended at the end of that stream, so their
/// numbers are interpreted under `ctm`. Glyph classification already
/// uses page space; this inverse map keeps the opaque block aligned
/// with the removed content. If `ctm` is singular / non-finite, the
/// region is returned unchanged (identity CTM is the common case and
/// is invertible; a degenerate leftover CTM cannot be corrected here).
pub fn region_in_stream_space(region: &RedactionRegion, ctm: &Matrix) -> RedactionRegion {
    if ctm == &Matrix::identity() {
        return *region;
    }
    let Some(inv) = invert_affine(ctm) else {
        return *region;
    };

    if let Some(qd) = region.quad {
        let mut out = [0.0_f32; 8];
        for i in 0..4 {
            let p = inv.transform_point(qd[i * 2], qd[i * 2 + 1]);
            out[i * 2] = p.x;
            out[i * 2 + 1] = p.y;
        }
        return RedactionRegion::from_quad(out, region.fill);
    }

    let page = Rect::from_points(
        region.bbox[0],
        region.bbox[1],
        region.bbox[2],
        region.bbox[3],
    );
    let local = transform_bbox(&page, &inv);
    RedactionRegion::from_rect(
        local.left(),
        local.top(),
        local.right(),
        local.bottom(),
        region.fill,
    )
}

/// Content-stream bytes drawing the opaque overlay for one region, or
/// empty when no overlay is to be drawn (no `IC` and
/// `draw_overlay_when_no_ic == false`).
///
/// Emits a self-contained `q … Q` block so it cannot leak graphics
/// state into surrounding (already-pruned) content. A rotated
/// `QuadPoints` region is filled as the exact quad polygon; otherwise
/// the normalized bbox rectangle is filled. The fill colour clamps to
/// `0.0..=1.0` (DeviceRGB).
pub fn region_overlay_ops(region: &RedactionRegion, opts: &RedactionOptions) -> Vec<u8> {
    let Some(fill) = resolved_fill(region, opts) else {
        return Vec::new();
    };
    let clamp = |c: f32| -> f32 {
        if c.is_nan() {
            0.0
        } else {
            c.clamp(0.0, 1.0)
        }
    };
    let (r, g, b) = (clamp(fill[0]), clamp(fill[1]), clamp(fill[2]));

    let mut out = String::new();
    out.push_str("q\n");
    let _ = writeln!(out, "{} {} {} rg", num(r), num(g), num(b));

    if let Some(qd) = region.quad {
        // Polygon path over the four QuadPoints corners.
        let _ = writeln!(out, "{} {} m", num(qd[0]), num(qd[1]));
        let _ = writeln!(out, "{} {} l", num(qd[2]), num(qd[3]));
        let _ = writeln!(out, "{} {} l", num(qd[4]), num(qd[5]));
        let _ = writeln!(out, "{} {} l", num(qd[6]), num(qd[7]));
        out.push_str("h\nf\n");
    } else {
        let [x0, y0, x1, y1] = region.bbox;
        let _ = writeln!(out, "{} {} {} {} re\nf", num(x0), num(y0), num(x1 - x0), num(y1 - y0));
    }
    out.push('Q');
    out.push('\n');
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::options::RedactionOptions;
    use crate::redaction::region::RedactionRegion;

    fn s(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn num_formats_pdf_reals_no_exponent() {
        assert_eq!(num(0.0), "0");
        assert_eq!(num(-0.0), "0");
        assert_eq!(num(12.0), "12");
        assert_eq!(num(12.5), "12.5");
        assert_eq!(num(0.10000), "0.1");
        assert_eq!(num(1.0e-7), "0"); // rounds to 0 at 4dp, not "1e-7"
        assert_eq!(num(f32::NAN), "0");
        assert_eq!(num(f32::INFINITY), "0");
        assert_eq!(num(-3.25), "-3.25");
    }

    #[test]
    fn rect_region_with_ic_emits_fill_block() {
        let region = RedactionRegion::from_rect(10.0, 20.0, 110.0, 70.0, Some([1.0, 0.0, 0.0]));
        let ops = s(&region_overlay_ops(&region, &RedactionOptions::default()));
        assert_eq!(ops, "q\n1 0 0 rg\n10 20 100 50 re\nf\nQ\n");
    }

    #[test]
    fn no_ic_uses_default_fill_when_enabled() {
        let region = RedactionRegion::from_rect(0.0, 0.0, 5.0, 5.0, None);
        let opts = RedactionOptions::default(); // black, draw_when_no_ic=true
        let ops = s(&region_overlay_ops(&region, &opts));
        assert_eq!(ops, "q\n0 0 0 rg\n0 0 5 5 re\nf\nQ\n");
    }

    #[test]
    fn no_ic_and_disabled_emits_nothing() {
        let region = RedactionRegion::from_rect(0.0, 0.0, 5.0, 5.0, None);
        let opts = RedactionOptions {
            draw_overlay_when_no_ic: false,
            ..RedactionOptions::default()
        };
        assert!(region_overlay_ops(&region, &opts).is_empty());
    }

    #[test]
    fn quad_region_emits_polygon_path() {
        let quad = [50.0, 0.0, 100.0, 50.0, 50.0, 100.0, 0.0, 50.0];
        let region = RedactionRegion::from_quad(quad, Some([0.0, 0.0, 0.0]));
        let ops = s(&region_overlay_ops(&region, &RedactionOptions::default()));
        assert_eq!(ops, "q\n0 0 0 rg\n50 0 m\n100 50 l\n50 100 l\n0 50 l\nh\nf\nQ\n");
    }

    #[test]
    fn fill_components_are_clamped() {
        let region = RedactionRegion::from_rect(0.0, 0.0, 1.0, 1.0, Some([2.0, -1.0, 0.5]));
        let ops = s(&region_overlay_ops(&region, &RedactionOptions::default()));
        assert!(ops.contains("1 0 0.5 rg"), "got: {ops}");
    }

    #[test]
    fn block_is_self_contained_q_q() {
        let region = RedactionRegion::from_rect(0.0, 0.0, 1.0, 1.0, Some([0.0, 0.0, 0.0]));
        let ops = s(&region_overlay_ops(&region, &RedactionOptions::default()));
        assert!(ops.starts_with("q\n") && ops.ends_with("Q\n"));
    }

    #[test]
    fn y_flip_ctm_maps_page_bbox_to_stream_space() {
        // Same page-level flip as Word/LibreOffice exports.
        let ctm = Matrix {
            a: 1.0,
            b: 0.0,
            c: 0.0,
            d: -1.0,
            e: 0.0,
            f: 792.0,
        };
        let page = RedactionRegion::from_rect(90.0, 100.0, 400.0, 140.0, Some([1.0, 0.0, 0.0]));
        let stream = region_in_stream_space(&page, &ctm);
        // (x, y) → (x, 792 - y): [90,100,400,140] → [90,652,400,692]
        assert_eq!(stream.bbox[0], 90.0);
        assert_eq!(stream.bbox[2], 400.0);
        assert!((stream.bbox[1] - 652.0).abs() < 0.01, "y0={}", stream.bbox[1]);
        assert!((stream.bbox[3] - 692.0).abs() < 0.01, "y1={}", stream.bbox[3]);
    }

    #[test]
    fn identity_ctm_leaves_region_unchanged() {
        let page = RedactionRegion::from_rect(10.0, 20.0, 30.0, 40.0, None);
        assert_eq!(region_in_stream_space(&page, &Matrix::identity()), page);
    }
}
