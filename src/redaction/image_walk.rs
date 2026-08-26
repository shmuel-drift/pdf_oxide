//! Record `Do` + CTM-at-op and intersecting inline images from the
//! existing graphics-state walk (same `apply_ctm` as text redaction).
//!
//! Form recursion and clone/rebind live in the editor — this module
//! does not load XObjects.

use super::classify::apply_ctm;
use super::image_prune::{classify_image_placement, ImageRedaction};
use super::region::RegionSet;
use crate::content::graphics_state::{GraphicsStateStack, Matrix};
use crate::content::operators::Operator;

/// One `Do` operator and the CTM active at that operator.
#[derive(Debug, Clone)]
pub struct DoPlacement {
    /// Index into the operator list (for rewriting that `Do`).
    pub index: usize,
    /// Resource name (`/Im1`, `/Fm1`, …).
    pub name: String,
    /// CTM at the operator (page space).
    pub ctm: Matrix,
}

/// Result of walking one content stream for image-relevant operators.
#[derive(Debug, Clone, Default)]
pub struct ImageWalk {
    /// Every `Do` in stream order (Image and Form; caller resolves subtype).
    pub dos: Vec<DoPlacement>,
    /// An inline `BI…EI` placement intersects a redaction region.
    pub intersecting_inline: bool,
}

/// Walk `ops` with `initial_ctm`, recording `Do` placements and whether
/// any inline image intersects `regions`.
///
/// `q` / `Q` / `cm` are tracked via [`apply_ctm`] — the same stack the
/// text engine uses. No second CTM implementation.
pub fn walk_stream_images(
    ops: &[Operator],
    initial_ctm: Matrix,
    regions: &RegionSet,
    min_padding: f32,
) -> ImageWalk {
    let mut stack = GraphicsStateStack::new();
    stack.current_mut().ctm = initial_ctm;
    let mut out = ImageWalk::default();

    for (index, op) in ops.iter().enumerate() {
        match op {
            Operator::SaveState | Operator::RestoreState | Operator::Cm { .. } => {
                apply_ctm(&mut stack, op);
            },
            Operator::Do { name } => {
                out.dos.push(DoPlacement {
                    index,
                    name: name.clone(),
                    ctm: stack.current().ctm,
                });
            },
            Operator::InlineImage { .. } => {
                let class = classify_image_placement(&stack.current().ctm, regions, min_padding);
                if class != ImageRedaction::Keep {
                    out.intersecting_inline = true;
                }
            },
            _ => {},
        }
    }
    out
}

/// Parse a Form `/Matrix` (default identity). Numbers may be int or real.
pub fn form_matrix_from_dict(
    dict: &std::collections::HashMap<String, crate::object::Object>,
) -> Matrix {
    use crate::object::Object;
    let Some(Object::Array(arr)) = dict.get("Matrix") else {
        return Matrix::identity();
    };
    if arr.len() != 6 {
        return Matrix::identity();
    }
    let num = |o: &Object| -> f32 {
        o.as_integer()
            .map(|i| i as f32)
            .or_else(|| o.as_real().map(|r| r as f32))
            .unwrap_or(0.0)
    };
    Matrix {
        a: num(&arr[0]),
        b: num(&arr[1]),
        c: num(&arr[2]),
        d: num(&arr[3]),
        e: num(&arr[4]),
        f: num(&arr[5]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redaction::region::{RedactionRegion, DEFAULT_EDGE_PADDING};

    fn regions_covering_origin() -> RegionSet {
        let mut rs = RegionSet::new(0);
        rs.push(RedactionRegion::from_rect(0.0, 0.0, 10.0, 10.0, None));
        rs
    }

    #[test]
    fn records_do_after_cm() {
        let ops = vec![
            Operator::Cm {
                a: 100.0,
                b: 0.0,
                c: 0.0,
                d: 100.0,
                e: 0.0,
                f: 0.0,
            },
            Operator::Do {
                name: "Im1".to_string(),
            },
        ];
        let walk =
            walk_stream_images(&ops, Matrix::identity(), &RegionSet::new(0), DEFAULT_EDGE_PADDING);
        assert_eq!(walk.dos.len(), 1);
        assert_eq!(walk.dos[0].name, "Im1");
        assert!((walk.dos[0].ctm.a - 100.0).abs() < f32::EPSILON);
        assert!(!walk.intersecting_inline);
    }

    #[test]
    fn intersecting_inline_sets_flag() {
        let ops = vec![
            Operator::Cm {
                a: 20.0,
                b: 0.0,
                c: 0.0,
                d: 20.0,
                e: 0.0,
                f: 0.0,
            },
            Operator::InlineImage {
                dict: Box::new(std::collections::HashMap::new()),
                data: vec![0, 1, 2, 3],
            },
        ];
        let walk = walk_stream_images(&ops, Matrix::identity(), &regions_covering_origin(), 0.0);
        assert!(walk.intersecting_inline);
    }

    #[test]
    fn form_matrix_default_identity() {
        let d = std::collections::HashMap::new();
        let m = form_matrix_from_dict(&d);
        assert!(m.is_identity());
    }
}
