//! Fail-closed when a redaction box hits vector paint (not scans).

use pdf_oxide::editor::{DocumentEditor, SaveOptions};
use pdf_oxide::RedactionOptions;

fn assemble_pdf(bodies: &[Vec<u8>]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::with_capacity(bodies.len() + 1);
    offsets.push(0u32);
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(out.len() as u32);
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        if !body.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"endobj\n");
    }
    let xref = out.len();
    let n = bodies.len() + 1;
    out.extend_from_slice(format!("xref\n0 {n}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in offsets.iter().skip(1) {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {n} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

fn stream_obj(dict: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("<< {dict} /Length {} >>\nstream\n", data.len()).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream\n");
    out
}

fn page_pdf(
    mediabox: &str,
    contents: &[u8],
    extra_objects: &[Vec<u8>],
    resources: &str,
) -> Vec<u8> {
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox {mediabox} \
             /Contents 4 0 R /Resources << {resources} >> >>\n"
        )
        .into_bytes(),
        stream_obj("", contents),
    ];
    bodies.extend(extra_objects.iter().cloned());
    assemble_pdf(&bodies)
}

fn save_raw(ed: &mut DocumentEditor) -> Vec<u8> {
    ed.save_to_bytes_with_options(SaveOptions {
        compress: false,
        ..SaveOptions::full_rewrite()
    })
    .expect("save")
}

#[test]
fn intersecting_stroked_line_fails_no_mutation() {
    let contents = b"10 40 m 40 40 l S";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src.clone()).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("path under box must fail");
    let msg = err.to_string();
    assert!(msg.contains("vector path"), "{msg}");
    let out = save_raw(&mut ed);
    assert!(
        out.windows(contents.len())
            .any(|w| w == contents.as_slice()),
        "failed apply must leave original path bytes"
    );
}

#[test]
fn path_outside_box_allows_apply() {
    let contents = b"80 80 m 90 80 l S";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 20.0, 20.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("path outside box");
}

#[test]
fn page_clip_does_not_fail_closed() {
    let contents = b"0 0 100 100 re W n";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [10.0, 10.0, 30.0, 30.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("clip rect must not refuse");
}

#[test]
fn filled_chart_rect_under_box_fails() {
    let contents = b"5 5 40 40 re f";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    assert!(ed
        .apply_redactions_destructive(RedactionOptions::default())
        .is_err());
}

#[test]
fn clip_then_fill_fails_no_mutation() {
    let contents = b"5 5 40 40 re W f";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src.clone()).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    assert!(ed
        .apply_redactions_destructive(RedactionOptions::default())
        .is_err());
    let out = save_raw(&mut ed);
    assert!(
        out.windows(contents.len())
            .any(|w| w == contents.as_slice()),
        "failed apply must leave original clip+fill bytes"
    );
}

#[test]
fn path_in_form_under_box_fails() {
    let form_stream = stream_obj(
        "/Type /XObject /Subtype /Form /BBox [0 0 50 50] /Matrix [1 0 0 1 0 0]",
        b"0 0 40 40 re f",
    );
    let contents = b"q 1 0 0 1 0 0 cm /Fm1 Do Q";
    let src = page_pdf("[0 0 100 100]", contents, &[form_stream], "/XObject << /Fm1 5 0 R >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("form path must fail");
    assert!(err.to_string().contains("vector path"), "{err}");
}

#[test]
fn shading_without_bbox_fails() {
    let shading = b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] \
         /Function 6 0 R >>\n"
        .to_vec();
    // Minimal sampled function so the file parses; apply must refuse before
    // evaluating it.
    let func = b"<< /FunctionType 2 /Domain [0 1] /C0 [0 0 0] /C1 [1 0 0] /N 1 >>\n".to_vec();
    let contents = b"/Sh1 sh";
    let src = page_pdf("[0 0 100 100]", contents, &[shading, func], "/Shading << /Sh1 5 0 R >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("shading must fail");
    assert!(err.to_string().contains("shading"), "{err}");
}

#[test]
fn close_and_stroke_s_fails_no_mutation() {
    let contents = b"10 10 m 40 10 l 40 40 l s";
    let src = page_pdf("[0 0 100 100]", contents, &[], "");
    let mut ed = DocumentEditor::from_bytes(src.clone()).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("s under box must fail");
    assert!(err.to_string().contains("vector path"), "{err}");
    let out = save_raw(&mut ed);
    assert!(
        out.windows(contents.len())
            .any(|w| w == contents.as_slice()),
        "failed apply must leave original s-path bytes"
    );
}

#[test]
fn gs_lw_fat_stroke_fails() {
    let contents = b"/GS1 gs 0.5 0 0 0.5 0 0 cm 0 110 m 20 110 l S";
    let src = page_pdf("[0 0 100 100]", contents, &[], "/ExtGState << /GS1 << /LW 20 >> >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    assert!(ed
        .apply_redactions_destructive(RedactionOptions::default())
        .is_err());
}

#[test]
fn gs_ca_only_path_outside_allows_apply() {
    let contents = b"/GS1 gs 80 80 m 90 80 l S";
    let src = page_pdf("[0 0 100 100]", contents, &[], "/ExtGState << /GS1 << /CA 0.5 >> >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 20.0, 20.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("alpha-only gs must not refuse an outside stroke");
}

#[test]
fn form_page_scale_form_translate_maps_path() {
    // Same compose as image-burn: Form /Matrix T(2,1) then page S(32)
    // maps a unit square onto [64,32]–[96,64]. Swapped order is [2,1]–[34,33].
    let page_c = b"q 32 0 0 32 0 0 cm /Fm1 Do Q";
    let form_c = b"0 0 1 1 re f";
    let form_stream =
        stream_obj("/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Matrix [1 0 0 1 2 1]", form_c);
    let src = page_pdf("[0 0 120 80]", page_c, &[form_stream], "/XObject << /Fm1 5 0 R >>");

    let mut ed = DocumentEditor::from_bytes(src.clone()).unwrap();
    ed.add_redaction(0, [64.0, 32.0, 96.0, 64.0], None).unwrap();
    assert!(
        ed.apply_redactions_destructive(RedactionOptions::default())
            .is_err(),
        "T then S must map the fill onto [64,32,96,64]"
    );

    let mut ed_wrong = DocumentEditor::from_bytes(src).unwrap();
    ed_wrong
        .add_redaction(0, [2.0, 1.0, 34.0, 33.0], None)
        .unwrap();
    ed_wrong
        .apply_redactions_destructive(RedactionOptions::default())
        .expect("S then T rect must not hit the fill under this crate's multiply");
}

#[test]
fn shading_with_matrix_fails_even_if_bbox_misses() {
    let shading = b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 100 0] \
         /BBox [200 200 250 250] /Matrix [1 0 0 1 0 0] /Function 6 0 R >>\n"
        .to_vec();
    let func = b"<< /FunctionType 2 /Domain [0 1] /C0 [0 0 0] /C1 [1 0 0] /N 1 >>\n".to_vec();
    let contents = b"/Sh1 sh";
    let src = page_pdf("[0 0 100 100]", contents, &[shading, func], "/Shading << /Sh1 5 0 R >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 50.0, 50.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("shading /Matrix is unprovable");
    assert!(err.to_string().contains("shading"), "{err}");
}

#[test]
fn typed_text_plus_underline_in_same_box_saves() {
    let contents = b"10 698 m 160 698 l S\nBT\n/F1 10 Tf\n1 0 0 1 100 700 Tm\n(TOPSECRET) Tj\nET\n";
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n".to_vec();
    let src = page_pdf("[0 0 612 792]", contents, &[font], "/Font << /F1 5 0 R >>");
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [90.0, 695.0, 160.0, 715.0], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("underline must not block stripped text");
    let out = save_raw(&mut ed);
    let doc = pdf_oxide::PdfDocument::from_bytes(out.clone()).unwrap();
    let text = doc.extract_text(0).unwrap_or_default();
    if text.is_empty() {
        assert!(
            !out.windows(9).any(|w| w == b"TOPSECRET"),
            "secret bytes still present without extracted text"
        );
    } else {
        assert!(!text.contains("TOPSECRET"), "secret still extractable: {text}");
    }
    assert!(
        out.windows(3).any(|w| w == b"698"),
        "underline stroke and its 698 coordinates must survive"
    );
}
