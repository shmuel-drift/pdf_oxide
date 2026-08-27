//! Detect vector paint (and shadings) that intersect a redaction region.
//!
//! Path geometry is **not** destroyed (`path_prune` is still planning-only).
//! An opaque overlay would fake a redaction while `m`/`l`/`re`/`S`/`f`
//! stay in the stream. Intersecting paint therefore **fails closed**.
//!
//! Clip-and-discard (`W`/`W*` then `n`) is ignored: page-sized clip rects
//! are ubiquitous and are not the secret drawing. Clip-and-paint
//! (`W` then `f`/`S`) still fails closed — clip does not end the path.

use super::classify::{apply_ctm, classify, transform_bbox};
use super::path_prune::polygon_bbox;
use super::region::RegionSet;
use crate::content::graphics_state::{GraphicsStateStack, Matrix};
use crate::content::operators::Operator;
use crate::error::{Error, Result};
use crate::geometry::{Point, Rect};
use std::collections::HashMap;

/// Unburnable mark that would be overlay-only if apply succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnburnableMark {
    /// Stroked or filled path geometry under a region.
    Path,
    /// `sh` shading whose bbox intersects, or whose bbox cannot be proved
    /// outside the region.
    Shading,
}

impl UnburnableMark {
    /// [`Error::Unsupported`] so apply rolls back with no output PDF.
    pub fn into_error(self) -> Error {
        match self {
            UnburnableMark::Path => Error::Unsupported(
                "destructive redaction cannot destroy intersecting vector paths; \
                 refusing rather than overlay-only"
                    .to_string(),
            ),
            UnburnableMark::Shading => Error::Unsupported(
                "destructive redaction cannot destroy intersecting shadings (sh); \
                 refusing rather than overlay-only"
                    .to_string(),
            ),
        }
    }
}

struct PathAcc {
    /// Path construction coordinates (user space at each `m`/`l`/`re`).
    local: Vec<Point>,
    /// Same points mapped through the CTM **at construction** (what
    /// extractors that transform on `m`/`l` see in the stream).
    page_at_construct: Vec<Point>,
    current: Option<Point>,
}

impl PathAcc {
    fn new() -> Self {
        Self {
            local: Vec::new(),
            page_at_construct: Vec::new(),
            current: None,
        }
    }

    fn clear(&mut self) {
        self.local.clear();
        self.page_at_construct.clear();
        self.current = None;
    }

    fn add(&mut self, p: Point, ctm: &Matrix) {
        self.local.push(p);
        let mapped = ctm.transform_point(p.x, p.y);
        self.page_at_construct.push(mapped);
        self.current = Some(p);
    }
}

fn inflate(r: Rect, pad: f32) -> Rect {
    if !(pad.is_finite()) || pad <= 0.0 {
        return r;
    }
    Rect::from_points(r.left() - pad, r.top() - pad, r.right() + pad, r.bottom() + pad)
}

fn points_bbox(pts: &[Point]) -> Option<Rect> {
    match pts.len() {
        0 => None,
        1 => {
            let p = pts[0];
            Some(Rect::from_points(p.x, p.y, p.x, p.y))
        },
        _ => polygon_bbox(pts),
    }
}

/// Hit if *either* construction-time page mapping or paint-time CTM
/// mapping intersects. Inflate the **page-space** envelope by stroke
/// extent so downscaling CTMs cannot shrink the pad.
fn path_hits(
    acc: &PathAcc,
    paint_ctm: &Matrix,
    regions: &RegionSet,
    padding: f32,
    stroke_pad_page: f32,
) -> bool {
    let Some(local) = points_bbox(&acc.local) else {
        return false;
    };
    let paint_page = transform_bbox(&local, paint_ctm);
    let page = match points_bbox(&acc.page_at_construct) {
        Some(c) => paint_page.union(&c),
        None => paint_page,
    };
    classify(&inflate(page, stroke_pad_page), &Matrix::identity(), regions, padding).is_affected()
}

/// `true` when stroked/filled path geometry or an unprovable/`sh` hit
/// intersects `regions`.
///
/// `shading_bbox` maps resource names to a local-space `/BBox` when known.
/// A `sh` whose name is missing from the map, or whose bbox intersects,
/// is unburnable. Omit the map (`None`) to refuse every `sh`.
pub fn intersecting_unburnable(
    ops: &[Operator],
    initial_ctm: Matrix,
    regions: &RegionSet,
    padding: f32,
    shading_bbox: Option<&HashMap<String, Rect>>,
) -> Option<UnburnableMark> {
    if regions.is_empty() {
        return None;
    }
    let mut stack = GraphicsStateStack::new();
    stack.current_mut().ctm = initial_ctm;
    let mut path = PathAcc::new();

    let stroke_pad_page = |stack: &GraphicsStateStack| {
        let w = stack.current().line_width.max(0.0);
        0.5 * w * stack.current().ctm.stroke_scale()
    };

    for op in ops {
        let ctm = stack.current().ctm;
        match op {
            Operator::SaveState | Operator::RestoreState | Operator::Cm { .. } => {
                apply_ctm(&mut stack, op);
            },
            Operator::SetLineWidth { width } => {
                stack.current_mut().line_width = *width;
            },
            Operator::MoveTo { x, y } => path.add(Point::new(*x, *y), &ctm),
            Operator::LineTo { x, y } => path.add(Point::new(*x, *y), &ctm),
            Operator::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            } => {
                path.add(Point::new(*x1, *y1), &ctm);
                path.add(Point::new(*x2, *y2), &ctm);
                path.add(Point::new(*x3, *y3), &ctm);
            },
            Operator::CurveToV { x2, y2, x3, y3 } => {
                if let Some(c) = path.current {
                    path.add(c, &ctm);
                }
                path.add(Point::new(*x2, *y2), &ctm);
                path.add(Point::new(*x3, *y3), &ctm);
            },
            Operator::CurveToY { x1, y1, x3, y3 } => {
                path.add(Point::new(*x1, *y1), &ctm);
                path.add(Point::new(*x3, *y3), &ctm);
            },
            Operator::Rectangle {
                x,
                y,
                width,
                height,
            } => {
                let x1 = *x + *width;
                let y1 = *y + *height;
                path.add(Point::new(*x, *y), &ctm);
                path.add(Point::new(x1, *y), &ctm);
                path.add(Point::new(x1, y1), &ctm);
                path.add(Point::new(*x, y1), &ctm);
            },
            Operator::ClosePath => {},
            Operator::Stroke => {
                if path_hits(&path, &stack.current().ctm, regions, padding, stroke_pad_page(&stack))
                {
                    return Some(UnburnableMark::Path);
                }
                path.clear();
            },
            Operator::Fill | Operator::FillEvenOdd => {
                if path_hits(&path, &stack.current().ctm, regions, padding, 0.0) {
                    return Some(UnburnableMark::Path);
                }
                path.clear();
            },
            Operator::FillStroke
            | Operator::FillStrokeEvenOdd
            | Operator::CloseFillStroke
            | Operator::CloseFillStrokeEvenOdd => {
                if path_hits(&path, &stack.current().ctm, regions, padding, stroke_pad_page(&stack))
                {
                    return Some(UnburnableMark::Path);
                }
                path.clear();
            },
            Operator::ClipNonZero | Operator::ClipEvenOdd => {
                // Clip does not end the path (ISO 32000-1 §8.5.4).
                // `re W n` discards on `n`; `re W f` still paints.
            },
            Operator::EndPath => {
                path.clear();
            },
            Operator::PaintShading { name } => match shading_bbox {
                None => return Some(UnburnableMark::Shading),
                Some(map) => match map.get(name) {
                    Some(local) => {
                        if classify(local, &stack.current().ctm, regions, padding).is_affected() {
                            return Some(UnburnableMark::Shading);
                        }
                    },
                    None => return Some(UnburnableMark::Shading),
                },
            },
            _ => {},
        }
    }
    None
}

/// Fail apply when `ops` paints unburnable geometry under `regions`.
pub fn refuse_intersecting_unburnable(
    ops: &[Operator],
    initial_ctm: Matrix,
    regions: &RegionSet,
    padding: f32,
    shading_bbox: Option<&HashMap<String, Rect>>,
) -> Result<()> {
    match intersecting_unburnable(ops, initial_ctm, regions, padding, shading_bbox) {
        Some(mark) => Err(mark.into_error()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::parser::parse_content_stream;
    use crate::redaction::region::{RedactionRegion, DEFAULT_EDGE_PADDING};

    fn box_origin() -> RegionSet {
        let mut rs = RegionSet::new(0);
        rs.push(RedactionRegion::from_rect(0.0, 0.0, 50.0, 50.0, None));
        rs
    }

    #[test]
    fn stroked_line_in_region_is_unburnable() {
        let ops = parse_content_stream(b"10 10 m 40 10 l S").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Path)
        );
    }

    #[test]
    fn stroked_line_outside_region_is_ok() {
        let ops = parse_content_stream(b"200 200 m 240 200 l S").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            None
        );
    }

    #[test]
    fn filled_rect_in_region_is_unburnable() {
        let ops = parse_content_stream(b"0 0 40 40 re f").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Path)
        );
    }

    #[test]
    fn page_clip_is_not_treated_as_drawing() {
        let ops = parse_content_stream(b"0 0 612 792 re W n").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            None
        );
    }

    #[test]
    fn shading_without_bbox_is_unburnable() {
        let ops = parse_content_stream(b"/Sh1 sh").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Shading)
        );
    }

    #[test]
    fn shading_bbox_outside_is_ok() {
        let ops = parse_content_stream(b"/Sh1 sh").unwrap();
        let mut map = HashMap::new();
        map.insert("Sh1".to_string(), Rect::from_points(200.0, 200.0, 250.0, 250.0));
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                Some(&map)
            ),
            None
        );
    }

    #[test]
    fn cm_places_path_into_region() {
        let ops = parse_content_stream(b"1 0 0 1 200 0 cm 10 10 m 20 10 l S").unwrap();
        let mut rs = RegionSet::new(0);
        rs.push(RedactionRegion::from_rect(200.0, 0.0, 250.0, 50.0, None));
        assert_eq!(
            intersecting_unburnable(&ops, Matrix::identity(), &rs, DEFAULT_EDGE_PADDING, None),
            Some(UnburnableMark::Path)
        );
    }

    #[test]
    fn clip_then_fill_is_unburnable() {
        let ops = parse_content_stream(b"0 0 40 40 re W f").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Path)
        );
    }

    #[test]
    fn construct_then_cm_then_stroke_still_sees_construction() {
        // Extractors map `m`/`l` at construction (inside the box). Paint CTM
        // then translates far away — fail closed on either interpretation.
        let ops = parse_content_stream(b"10 10 m 40 10 l 1 0 0 1 1000 0 cm S").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Path)
        );
    }

    #[test]
    fn fat_stroke_downscale_pad_is_page_space() {
        // Spine at local y=110, cm scale 0.5 → page y=55 (outside 0..50).
        // Width 20 → page half-pad 5 → envelope 50..60 intersects the box.
        let ops = parse_content_stream(b"20 w 0.5 0 0 0.5 0 0 cm 0 110 m 20 110 l S").unwrap();
        assert_eq!(
            intersecting_unburnable(
                &ops,
                Matrix::identity(),
                &box_origin(),
                DEFAULT_EDGE_PADDING,
                None
            ),
            Some(UnburnableMark::Path)
        );
    }
}
