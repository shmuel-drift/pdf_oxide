//! Image-burn apply path: resolve XObjects, recurse Forms, clone/rebind,
//! G6 candidates. Called only after font refuse so `modified_objects`
//! is not written on Type0 failure.

use super::DocumentEditor;
use crate::content::graphics_state::Matrix;
use crate::content::operators::Operator;
use crate::error::{Error, Result};
use crate::extractors::extract_image_from_xobject;
use crate::geometry::Rect;
use crate::object::{Object, ObjectRef};
use crate::redaction::image_burn::{
    assert_image_burnable, burn_image_wipes, burned_xobject, MAX_FORM_DEPTH,
};
use crate::redaction::image_prune::{
    classify_image_placement, classify_image_wipes, regions_that_burn_this_image, ImageRedaction,
};
use crate::redaction::image_walk::{form_matrix_from_dict, walk_stream_images, DoPlacement};
use crate::redaction::path_walk::refuse_intersecting_unburnable;
use crate::redaction::region::{or_merge_flags, RegionSet};
use crate::redaction::serialize::serialize_operator;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Per-stream `/Resources/XObject` edits: insert/replace names, then drop
/// unused originals. Drop-then-insert so a same-name rebind is not deleted.
#[derive(Debug, Clone, Default)]
pub(super) struct XObjectPatch {
    pub rebinds: HashMap<String, ObjectRef>,
    pub drop_names: HashSet<String>,
}

impl XObjectPatch {
    pub(super) fn is_empty(&self) -> bool {
        self.rebinds.is_empty() && self.drop_names.is_empty()
    }

    pub(super) fn apply_to(&self, xo: &mut HashMap<String, Object>) {
        for name in &self.drop_names {
            xo.remove(name);
        }
        for (n, r) in &self.rebinds {
            xo.insert(n.clone(), Object::Reference(*r));
        }
    }
}

pub(super) struct BurnStreamResult {
    pub ops: Vec<Operator>,
    pub xobject_patch: XObjectPatch,
    pub images_modified: usize,
    pub xobjects_specialized: usize,
    pub replaced_ids: HashSet<u32>,
    /// One flag per redaction rectangle: this stream actually burned
    /// JPEG/Flate pixels in that box (set only after `burn_image_wipes`
    /// succeeds, including nested Forms).
    pub(crate) burned_pixels_by_region: Vec<bool>,
}

/// Mark boxes whose wipe set was written into this placement. Call only
/// after `burn_image_wipes` succeeds — a would-wipe that then fails
/// `assert_image_burnable` must not skip the path/`sh` check.
fn record_burned_pixels(
    burned_pixels_by_region: &mut [bool],
    ctm: &Matrix,
    regions: &RegionSet,
    padding: f32,
) {
    or_merge_flags(burned_pixels_by_region, &regions_that_burn_this_image(ctm, regions, padding));
}

impl DocumentEditor {
    fn load_obj(&self, r: ObjectRef) -> Result<Object> {
        if let Some(m) = self.modified_objects.get(&r.id) {
            Ok(m.clone())
        } else {
            self.source.load_object(r)
        }
    }

    fn as_dict_resolved(&self, obj: &Object) -> Result<HashMap<String, Object>> {
        match obj {
            Object::Dictionary(d) => Ok(d.clone()),
            Object::Reference(r) => self
                .load_obj(*r)?
                .as_dict()
                .cloned()
                .ok_or_else(|| Error::InvalidPdf("expected dictionary".to_string())),
            Object::Stream { dict, .. } => Ok(dict.clone()),
            _ => Err(Error::InvalidPdf("expected dictionary".to_string())),
        }
    }

    /// Page `/Resources`, walking `/Parent` (ISO inheritable attributes).
    pub(super) fn resolve_page_resources(
        &self,
        source_page: usize,
    ) -> Result<HashMap<String, Object>> {
        let page_ref = self.source.get_page_ref(source_page)?;
        let mut node = Some(self.load_obj(page_ref)?);
        let mut guard = 0;
        while let Some(obj) = node {
            guard += 1;
            if guard > 64 {
                return Err(Error::Unsupported(
                    "cyclic /Parent while resolving page /Resources".to_string(),
                ));
            }
            let Some(dict) = obj.as_dict() else { break };
            if let Some(res) = dict.get("Resources") {
                return self.as_dict_resolved(res);
            }
            node = dict
                .get("Parent")
                .and_then(Object::as_reference)
                .and_then(|r| self.load_obj(r).ok());
        }
        Ok(HashMap::new())
    }

    pub(super) fn xobject_entries(
        &self,
        resources: &HashMap<String, Object>,
    ) -> Result<HashMap<String, Object>> {
        match resources.get("XObject") {
            Some(obj) => self.as_dict_resolved(obj),
            None => Ok(HashMap::new()),
        }
    }

    fn color_space_map(
        &self,
        resources: &HashMap<String, Object>,
    ) -> Option<HashMap<String, Object>> {
        resources
            .get("ColorSpace")
            .and_then(|o| self.as_dict_resolved(o).ok())
    }

    /// `/Shading` resource name → `/BBox` when present and the dict has no
    /// `/Matrix`. Missing bbox or a `/Matrix` means `path_walk` cannot prove
    /// the shading is outside a region.
    fn shading_bboxes(&self, resources: &HashMap<String, Object>) -> HashMap<String, Rect> {
        let mut out = HashMap::new();
        let Some(sh_obj) = resources.get("Shading") else {
            return out;
        };
        let Ok(sh) = self.as_dict_resolved(sh_obj) else {
            return out;
        };
        for (name, val) in sh {
            let Ok(dict) = self.as_dict_resolved(&val) else {
                continue;
            };
            if dict.contains_key("Matrix") {
                continue;
            }
            let Some(Object::Array(arr)) = dict.get("BBox") else {
                continue;
            };
            if arr.len() != 4 {
                continue;
            }
            let num = |o: &Object| -> Option<f32> {
                o.as_integer()
                    .map(|i| i as f32)
                    .or_else(|| o.as_real().map(|r| r as f32))
            };
            let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                (num(&arr[0]), num(&arr[1]), num(&arr[2]), num(&arr[3]))
            else {
                continue;
            };
            out.insert(name, Rect::from_points(x0, y0, x1, y1).normalize());
        }
        out
    }

    /// `/ExtGState` name → `/LW` when present. Resolved dicts without `/LW`
    /// map to `None` (alpha-only `gs` is not a stroke-width hole). Names
    /// omitted from the map are unresolvable and fail closed.
    fn ext_gstate_line_widths(
        &self,
        resources: &HashMap<String, Object>,
    ) -> HashMap<String, Option<f32>> {
        let mut out = HashMap::new();
        let Some(gs_obj) = resources.get("ExtGState") else {
            return out;
        };
        let Ok(gs) = self.as_dict_resolved(gs_obj) else {
            return out;
        };
        for (name, val) in gs {
            let Ok(dict) = self.as_dict_resolved(&val) else {
                continue;
            };
            let lw = dict.get("LW").and_then(|o| {
                o.as_integer()
                    .map(|i| i as f32)
                    .or_else(|| o.as_real().map(|r| r as f32))
            });
            out.insert(name, lw);
        }
        out
    }

    fn resolve_xobject(
        &self,
        resources: &HashMap<String, Object>,
        name: &str,
    ) -> Result<(Option<ObjectRef>, Object)> {
        let xo = self.xobject_entries(resources)?;
        match xo.get(name) {
            Some(Object::Reference(r)) => Ok((Some(*r), self.load_obj(*r)?)),
            Some(obj) => Ok((None, obj.clone())),
            None => Err(Error::Unsupported(format!(
                "content stream Do /{name} has no XObject resource"
            ))),
        }
    }

    fn unique_xobject_name(base: &str, used: &HashSet<String>) -> String {
        let mut i = 0u32;
        loop {
            let n = format!("{base}_R{i}");
            if !used.contains(&n) {
                return n;
            }
            i += 1;
        }
    }

    fn decode_xobject_stream(&self, obj: &Object, obj_ref: Option<ObjectRef>) -> Result<Vec<u8>> {
        if let Some(r) = obj_ref {
            self.source.decode_stream_with_encryption(obj, r)
        } else {
            obj.decode_stream_data()
        }
    }

    fn xobject_subtype(obj: &Object) -> &str {
        obj.as_dict()
            .and_then(|d| d.get("Subtype"))
            .and_then(Object::as_name)
            .unwrap_or("")
    }

    /// Drop the original resource name only when every placement was cloned
    /// onto a new name (`len != 1`). The single-placement path rebinds the
    /// same name instead.
    fn drop_original_if_all_cloned(
        patch: &mut XObjectPatch,
        replaced_ids: &mut HashSet<u32>,
        name: &str,
        obj_ref: Option<ObjectRef>,
        affected_len: usize,
        placements_len: usize,
    ) {
        if affected_len == placements_len && placements_len != 1 {
            patch.drop_names.insert(name.to_string());
            if let Some(r) = obj_ref {
                replaced_ids.insert(r.id);
            }
        }
    }

    pub(super) fn burn_stream(
        &mut self,
        ops: Vec<Operator>,
        resources: &HashMap<String, Object>,
        initial_ctm: Matrix,
        regions: &RegionSet,
        padding: f32,
        visiting: &mut HashSet<u32>,
        depth: u32,
    ) -> Result<BurnStreamResult> {
        if depth > MAX_FORM_DEPTH {
            return Err(Error::Unsupported(format!(
                "Form XObject nesting exceeds {MAX_FORM_DEPTH}"
            )));
        }
        // Path/`sh`/`gs` refuse runs after this function returns, and only
        // for boxes that destroyed neither glyphs nor pixels. Inline `BI`
        // still fails here: there is no burn path for it.
        let walk = walk_stream_images(&ops, initial_ctm, regions, padding);
        if walk.intersecting_inline {
            return Err(Error::Unsupported("redaction cannot burn inline (BI) images".to_string()));
        }

        let mut ops = ops;
        let mut xobject_patch = XObjectPatch::default();
        let mut images_modified = 0usize;
        let mut xobjects_specialized = 0usize;
        let mut replaced_ids: HashSet<u32> = HashSet::new();
        let mut burned_pixels_by_region = vec![false; regions.len()];

        let mut used_names: HashSet<String> =
            self.xobject_entries(resources)?.keys().cloned().collect();

        let mut by_name: BTreeMap<String, Vec<DoPlacement>> = BTreeMap::new();
        for d in walk.dos {
            by_name.entry(d.name.clone()).or_default().push(d);
        }

        for (name, placements) in by_name {
            let (obj_ref, obj) = self.resolve_xobject(resources, &name)?;
            match Self::xobject_subtype(&obj) {
                "Form" => {
                    let form_dict = obj
                        .as_dict()
                        .ok_or_else(|| Error::InvalidPdf("Form is not a stream".to_string()))?;
                    let form_matrix = form_matrix_from_dict(form_dict);
                    let form_res = if let Some(res_obj) = form_dict.get("Resources") {
                        self.as_dict_resolved(res_obj)?
                    } else {
                        resources.clone()
                    };
                    let form_bytes = self.decode_xobject_stream(&obj, obj_ref)?;
                    let form_ops = crate::content::parser::parse_content_stream(&form_bytes)?;

                    let mut inners: Vec<(DoPlacement, BurnStreamResult)> = Vec::new();
                    for p in &placements {
                        if let Some(r) = obj_ref {
                            if !visiting.insert(r.id) {
                                return Err(Error::Unsupported(format!(
                                    "cyclic Form XObject {} while redacting",
                                    r.id
                                )));
                            }
                        }
                        let composed = form_matrix.multiply(&p.ctm);
                        let inner = self.burn_stream(
                            form_ops.clone(),
                            &form_res,
                            composed,
                            regions,
                            padding,
                            visiting,
                            depth + 1,
                        );
                        if let Some(r) = obj_ref {
                            visiting.remove(&r.id);
                        }
                        let inner = inner?;
                        or_merge_flags(
                            &mut burned_pixels_by_region,
                            &inner.burned_pixels_by_region,
                        );
                        inners.push((p.clone(), inner));
                    }

                    let affected: Vec<(DoPlacement, BurnStreamResult)> = inners
                        .into_iter()
                        .filter(|(_, inner)| {
                            inner.images_modified > 0
                                || inner.xobjects_specialized > 0
                                || !inner.xobject_patch.is_empty()
                        })
                        .collect();
                    if affected.is_empty() {
                        continue;
                    }

                    let all_affected = affected.len() == placements.len();
                    if all_affected && placements.len() == 1 {
                        let Some((p, inner)) = affected.into_iter().next() else {
                            continue;
                        };
                        let cloned = self.clone_form_with_inner(&obj, &form_res, inner)?;
                        images_modified += cloned.images_modified;
                        xobjects_specialized += cloned.xobjects_specialized + 1;
                        replaced_ids.extend(cloned.replaced_ids);
                        if let Some(r) = obj_ref {
                            replaced_ids.insert(r.id);
                        }
                        let new_id = self.allocate_object_id();
                        self.insert_modified(new_id, cloned.form);
                        xobject_patch
                            .rebinds
                            .insert(p.name, ObjectRef::new(new_id, 0));
                    } else {
                        let affected_len = affected.len();
                        for (p, inner) in affected {
                            let cloned = self.clone_form_with_inner(&obj, &form_res, inner)?;
                            images_modified += cloned.images_modified;
                            xobjects_specialized += cloned.xobjects_specialized + 1;
                            replaced_ids.extend(cloned.replaced_ids);
                            let new_id = self.allocate_object_id();
                            self.insert_modified(new_id, cloned.form);
                            let new_ref = ObjectRef::new(new_id, 0);
                            let new_name = Self::unique_xobject_name(&name, &used_names);
                            used_names.insert(new_name.clone());
                            xobject_patch.rebinds.insert(new_name.clone(), new_ref);
                            if let Operator::Do { name: n } = &mut ops[p.index] {
                                *n = new_name;
                            }
                        }
                        Self::drop_original_if_all_cloned(
                            &mut xobject_patch,
                            &mut replaced_ids,
                            &name,
                            obj_ref,
                            affected_len,
                            placements.len(),
                        );
                    }
                },
                "Image" => {
                    let classified: Vec<(DoPlacement, Vec<ImageRedaction>)> = placements
                        .iter()
                        .map(|p| (p.clone(), classify_image_wipes(&p.ctm, regions, padding)))
                        .collect();
                    let affected: Vec<(DoPlacement, Vec<ImageRedaction>)> = classified
                        .iter()
                        .filter(|(_, w)| w.iter().any(|c| *c != ImageRedaction::Keep))
                        .cloned()
                        .collect();
                    if affected.is_empty() {
                        continue;
                    }

                    let dict = obj
                        .as_dict()
                        .ok_or_else(|| Error::InvalidPdf("Image is not a stream".to_string()))?;
                    assert_image_burnable(dict)?;
                    let cs_map = self.color_space_map(resources);
                    let extracted = extract_image_from_xobject(
                        Some(&self.source),
                        &obj,
                        obj_ref,
                        cs_map.as_ref(),
                    )?;

                    // One placement: rebind the original name. Several: clone
                    // each so wipe sets do not merge into one JPEG.
                    if placements.len() == 1 {
                        let Some((p, wipes)) = affected.into_iter().next() else {
                            continue;
                        };
                        let burned = burn_image_wipes(&extracted, wipes)?;
                        record_burned_pixels(
                            &mut burned_pixels_by_region,
                            &p.ctm,
                            regions,
                            padding,
                        );
                        let new_id = self.allocate_object_id();
                        self.insert_modified(new_id, burned_xobject(burned));
                        xobject_patch
                            .rebinds
                            .insert(name, ObjectRef::new(new_id, 0));
                        if let Some(r) = obj_ref {
                            replaced_ids.insert(r.id);
                        }
                        images_modified += 1;
                        xobjects_specialized += 1;
                    } else {
                        let affected_len = affected.len();
                        for (p, wipes) in affected {
                            let burned = burn_image_wipes(&extracted, wipes)?;
                            record_burned_pixels(
                                &mut burned_pixels_by_region,
                                &p.ctm,
                                regions,
                                padding,
                            );
                            let new_id = self.allocate_object_id();
                            self.insert_modified(new_id, burned_xobject(burned));
                            let new_name = Self::unique_xobject_name(&name, &used_names);
                            used_names.insert(new_name.clone());
                            xobject_patch
                                .rebinds
                                .insert(new_name.clone(), ObjectRef::new(new_id, 0));
                            if let Operator::Do { name: n } = &mut ops[p.index] {
                                *n = new_name;
                            }
                            images_modified += 1;
                            xobjects_specialized += 1;
                        }
                        Self::drop_original_if_all_cloned(
                            &mut xobject_patch,
                            &mut replaced_ids,
                            &name,
                            obj_ref,
                            affected_len,
                            placements.len(),
                        );
                    }
                },
                other => {
                    for p in &placements {
                        let class = classify_image_placement(&p.ctm, regions, padding);
                        if class != ImageRedaction::Keep {
                            return Err(Error::Unsupported(format!(
                                "redaction cannot burn XObject subtype {other:?}"
                            )));
                        }
                    }
                },
            }
        }

        Ok(BurnStreamResult {
            ops,
            xobject_patch,
            images_modified,
            xobjects_specialized,
            replaced_ids,
            burned_pixels_by_region,
        })
    }

    /// Fail closed on path/`sh`/unresolved `gs` under `regions`, including
    /// nested Form XObjects.
    ///
    /// `regions` is already the subset that destroyed neither glyphs nor
    /// pixels. Call this *after* `burn_stream` and pass page `/XObject`
    /// with the burn name patch applied: cloned Forms get new `Do` names,
    /// and walking the original names would miss paths inside those Forms.
    pub(super) fn refuse_unburnable_paint_including_forms(
        &self,
        ops: &[Operator],
        resources: &HashMap<String, Object>,
        initial_ctm: Matrix,
        regions: &RegionSet,
        padding: f32,
        visiting: &mut HashSet<u32>,
        depth: u32,
    ) -> Result<()> {
        if regions.is_empty() {
            return Ok(());
        }
        if depth > MAX_FORM_DEPTH {
            return Err(Error::Unsupported(format!(
                "Form XObject nesting exceeds {MAX_FORM_DEPTH}"
            )));
        }
        let shading = self.shading_bboxes(resources);
        let gs_lw = self.ext_gstate_line_widths(resources);
        refuse_intersecting_unburnable(
            ops,
            initial_ctm,
            regions,
            padding,
            Some(&shading),
            Some(&gs_lw),
        )?;
        let walk = walk_stream_images(ops, initial_ctm, regions, padding);
        for d in &walk.dos {
            let (obj_ref, obj) = self.resolve_xobject(resources, &d.name)?;
            if Self::xobject_subtype(&obj) != "Form" {
                continue;
            }
            let form_dict = obj
                .as_dict()
                .ok_or_else(|| Error::InvalidPdf("Form is not a stream".to_string()))?;
            let form_matrix = form_matrix_from_dict(form_dict);
            let form_res = if let Some(res_obj) = form_dict.get("Resources") {
                self.as_dict_resolved(res_obj)?
            } else {
                resources.clone()
            };
            let form_bytes = self.decode_xobject_stream(&obj, obj_ref)?;
            let form_ops = crate::content::parser::parse_content_stream(&form_bytes)?;
            if let Some(r) = obj_ref {
                if !visiting.insert(r.id) {
                    return Err(Error::Unsupported(format!(
                        "cyclic Form XObject {} while redacting",
                        r.id
                    )));
                }
            }
            let composed = form_matrix.multiply(&d.ctm);
            let inner = self.refuse_unburnable_paint_including_forms(
                &form_ops,
                &form_res,
                composed,
                regions,
                padding,
                visiting,
                depth + 1,
            );
            if let Some(r) = obj_ref {
                visiting.remove(&r.id);
            }
            inner?;
        }
        Ok(())
    }

    fn clone_form_with_inner(
        &self,
        form_obj: &Object,
        parent_or_form_res: &HashMap<String, Object>,
        inner: BurnStreamResult,
    ) -> Result<ClonedForm> {
        let mut body = Vec::new();
        for op in &inner.ops {
            serialize_operator(&mut body, op);
        }
        let mut dict = form_obj
            .as_dict()
            .cloned()
            .ok_or_else(|| Error::InvalidPdf("Form is not a stream".to_string()))?;
        let mut res = if let Some(res_obj) = dict.get("Resources") {
            self.as_dict_resolved(res_obj)?
        } else {
            parent_or_form_res.clone()
        };
        let mut xo = match res.get("XObject") {
            Some(x) => self.as_dict_resolved(x)?,
            None => HashMap::new(),
        };
        inner.xobject_patch.apply_to(&mut xo);
        res.insert("XObject".to_string(), Object::Dictionary(xo));
        dict.insert("Resources".to_string(), Object::Dictionary(res));
        dict.remove("Filter");
        dict.remove("DecodeParms");
        dict.insert("Length".to_string(), Object::Integer(body.len() as i64));
        Ok(ClonedForm {
            form: Object::Stream {
                dict,
                data: bytes::Bytes::from(body),
            },
            images_modified: inner.images_modified,
            xobjects_specialized: inner.xobjects_specialized,
            replaced_ids: inner.replaced_ids,
        })
    }

    pub(super) fn queue_drop_page_preview(&mut self, src: usize) -> Result<()> {
        let page_ref = self.source.get_page_ref(src)?;
        let page = self.load_obj(page_ref)?;
        if let Some(dict) = page.as_dict() {
            if let Some(Object::Reference(r)) = dict.get("Thumb") {
                self.redacted_orphan_ids.insert(r.id);
            }
            if let Some(Object::Array(arr)) = dict.get("Alternates") {
                for it in arr {
                    if let Some(r) = it.as_reference() {
                        self.redacted_orphan_ids.insert(r.id);
                    }
                }
            }
        }
        self.redacted_drop_preview.insert(src);
        Ok(())
    }

    pub(super) fn collect_live_xobject_ids(&self) -> HashSet<u32> {
        let mut live = HashSet::new();
        let mut visiting = HashSet::new();
        let n = self.original_page_count;
        for i in 0..n {
            if let Ok(res) = self.page_resources_with_rebinds(i) {
                self.collect_from_resources(&res, &mut live, &mut visiting);
            }
        }
        live
    }

    fn page_resources_with_rebinds(&self, page: usize) -> Result<HashMap<String, Object>> {
        let mut res = self.resolve_page_resources(page)?;
        if let Some(patch) = self.redacted_xobject_rebinds.get(&page) {
            let mut xo = self.xobject_entries(&res)?;
            patch.apply_to(&mut xo);
            res.insert("XObject".to_string(), Object::Dictionary(xo));
        }
        Ok(res)
    }

    fn collect_from_resources(
        &self,
        resources: &HashMap<String, Object>,
        live: &mut HashSet<u32>,
        visiting: &mut HashSet<u32>,
    ) {
        let Ok(xo) = self.xobject_entries(resources) else {
            return;
        };
        for obj in xo.values() {
            let Some(r) = obj.as_reference() else {
                continue;
            };
            live.insert(r.id);
            if !visiting.insert(r.id) {
                continue;
            }
            if let Ok(loaded) = self.load_obj(r) {
                if Self::xobject_subtype(&loaded) == "Form" {
                    if let Some(dict) = loaded.as_dict() {
                        if let Some(fres) = dict.get("Resources") {
                            if let Ok(fd) = self.as_dict_resolved(fres) {
                                self.collect_from_resources(&fd, live, visiting);
                            }
                        }
                    }
                }
            }
            visiting.remove(&r.id);
        }
    }

    pub(super) fn g6_unreferenced_replaced(&mut self, replaced: &HashSet<u32>) {
        let live = self.collect_live_xobject_ids();
        for id in replaced {
            if !live.contains(id) {
                self.redacted_orphan_ids.insert(*id);
            }
        }
        self.g6_unused_inlined_resource_dicts();
    }

    /// Save inlines rebound `/Resources` on image-burn pages. The old
    /// Resources dictionary still names the pre-burn image id (indirect
    /// `/Resources` → `/Im1 <id> 0 R`) and must not be emitted if no other
    /// page still points at it.
    fn g6_unused_inlined_resource_dicts(&mut self) {
        let n = self.original_page_count;
        let mut still_needed = HashSet::new();
        for i in 0..n {
            if self.redacted_xobject_rebinds.contains_key(&i) {
                continue;
            }
            if let Some(id) = self.page_resources_ref_id(i) {
                still_needed.insert(id);
            }
        }
        for i in 0..n {
            if !self.redacted_xobject_rebinds.contains_key(&i) {
                continue;
            }
            if let Some(id) = self.page_resources_ref_id(i) {
                if !still_needed.contains(&id) {
                    self.redacted_orphan_ids.insert(id);
                }
            }
        }
    }

    fn page_resources_ref_id(&self, page: usize) -> Option<u32> {
        let page_ref = self.source.get_page_ref(page).ok()?;
        let page = self.load_obj(page_ref).ok()?;
        match page.as_dict()?.get("Resources") {
            Some(Object::Reference(r)) => Some(r.id),
            _ => None,
        }
    }

    /// Copy-on-write `/Resources/XObject` rebinds and drop `/Thumb` +
    /// `/Alternates` on a page dict clone (never mutate a shared parent).
    pub(super) fn patch_page_dict_for_image_burn(
        &self,
        page_dict: &mut HashMap<String, Object>,
        page_index: usize,
    ) -> Result<()> {
        if self.redacted_drop_preview.contains(&page_index) {
            page_dict.remove("Thumb");
            page_dict.remove("Alternates");
        }
        let Some(patch) = self.redacted_xobject_rebinds.get(&page_index) else {
            return Ok(());
        };
        if patch.is_empty() {
            return Ok(());
        }
        let mut res = if let Some(existing) = page_dict.get("Resources") {
            self.as_dict_resolved(existing)?
        } else {
            self.resolve_page_resources(page_index)?
        };
        let mut xo = self.xobject_entries(&res)?;
        patch.apply_to(&mut xo);
        res.insert("XObject".to_string(), Object::Dictionary(xo));
        page_dict.insert("Resources".to_string(), Object::Dictionary(res));
        Ok(())
    }
}

struct ClonedForm {
    form: Object,
    images_modified: usize,
    xobjects_specialized: usize,
    replaced_ids: HashSet<u32>,
}
