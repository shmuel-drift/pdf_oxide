//! Integration tests for JPEG/Flate pixel burn (#231 image redaction).

use flate2::write::ZlibEncoder;
use flate2::Compression;
use image::codecs::jpeg::JpegEncoder;
use image::ImageEncoder;
use pdf_oxide::editor::{DocumentEditor, SaveOptions};
use pdf_oxide::{PdfDocument, RedactionOptions};
use std::io::Write;

const SECRET_R: u8 = 255;
const SECRET_G: u8 = 0;
const SECRET_B: u8 = 255; // magenta

fn encode_jpeg_rgb(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut buf, 95);
    encoder
        .write_image(pixels, width, height, image::ColorType::Rgb8.into())
        .expect("jpeg");
    buf
}

fn magenta_on_green(width: u32, height: u32) -> Vec<u8> {
    let mut p = Vec::with_capacity((width * height * 3) as usize);
    for y in 0..height {
        for x in 0..width {
            // Decoder-top center blob is the secret (PDF-top).
            let secret =
                x >= width / 4 && x < 3 * width / 4 && y >= height / 4 && y < 3 * height / 4;
            if secret {
                p.extend_from_slice(&[SECRET_R, SECRET_G, SECRET_B]);
            } else {
                p.extend_from_slice(&[0, 180, 0]);
            }
        }
    }
    p
}

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

fn image_xobject(filter: &str, w: u32, h: u32, cs: &str, bpc: u8, data: &[u8]) -> Vec<u8> {
    stream_obj(
        &format!(
            "/Type /XObject /Subtype /Image /Width {w} /Height {h} \
             /ColorSpace /{cs} /BitsPerComponent {bpc} /Filter /{filter}"
        ),
        data,
    )
}

/// One page, image fills MediaBox via `w 0 0 h 0 0 cm /Im1 Do`.
fn jpeg_page_pdf(
    w: u32,
    h: u32,
    jpeg: &[u8],
    extra_content: &str,
    thumb: Option<&[u8]>,
) -> Vec<u8> {
    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q{extra_content}");
    let thumb_entry = if thumb.is_some() { " /Thumb 6 0 R" } else { "" };
    let mut bodies = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >>{thumb_entry} >>\n"
        )
        .into_bytes(),
        stream_obj("/Type /Contents", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, jpeg),
    ];
    if let Some(t) = thumb {
        bodies.push(image_xobject("DCTDecode", 8, 8, "DeviceRGB", 8, t));
    }
    assemble_pdf(&bodies)
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn save_raw(ed: &mut DocumentEditor) -> Vec<u8> {
    ed.save_to_bytes_with_options(SaveOptions {
        compress: false,
        ..SaveOptions::full_rewrite()
    })
    .expect("save")
}

fn extracted_rgb(pdf: &[u8], page: usize) -> Vec<image::RgbImage> {
    let doc = PdfDocument::from_bytes(pdf.to_vec()).expect("open");
    doc.extract_images(page)
        .expect("extract_images")
        .into_iter()
        .filter_map(|im| im.to_dynamic_image().ok().map(|d| d.to_rgb8()))
        .collect()
}

fn pixel_near(p: &[u8; 3], r: u8, g: u8, b: u8, tol: u8) -> bool {
    p[0].abs_diff(r) <= tol && p[1].abs_diff(g) <= tol && p[2].abs_diff(b) <= tol
}

fn has_magenta(imgs: &[image::RgbImage]) -> bool {
    imgs.iter().any(|im| {
        im.pixels()
            .any(|p| pixel_near(&p.0, SECRET_R, SECRET_G, SECRET_B, 40))
    })
}

#[test]
fn jpeg_full_page_burn_destroys_secret_pixels() {
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let src = jpeg_page_pdf(w, h, &jpeg, "", None);
    assert!(has_magenta(&extracted_rgb(&src, 0)), "fixture must contain magenta secret");

    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    let report = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    assert!(report.images_modified >= 1, "report = {report:?}");
    let out = save_raw(&mut ed);
    let imgs = extracted_rgb(&out, 0);
    assert!(!has_magenta(&imgs), "magenta still recoverable from images");
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG stream must be absent from the file (G6)"
    );
}

#[test]
fn jpeg_indirect_resources_drops_original_stream() {
    // Indirect page /Resources dict. Burning the only Do must drop both
    // the original JPEG and that Resources object.
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q");
    let src = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 4 0 R /Resources 6 0 R >>\n"
        )
        .into_bytes(),
        stream_obj("/Type /Contents", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
        b"<< /XObject << /Im1 5 0 R >> >>\n".to_vec(),
    ]);
    assert!(contains_bytes(&src, &jpeg), "fixture must embed the original JPEG");

    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    assert!(
        !has_magenta(&extracted_rgb(&out, 0)),
        "magenta still recoverable from page images"
    );
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG must not survive as an unreferenced XObject"
    );
    // Old Resources dict serialized as `/Im1 5 0 R` naming the dropped stream.
    assert!(
        !contains_bytes(&out, b"/Im1 5 0 R"),
        "replaced page /Resources dict must not be emitted"
    );
}

#[test]
fn two_disjoint_rects_on_one_jpeg_leave_the_gap() {
    // 64×64 so MCU 8×8 pad + region padding cannot close a wide center gap.
    let w = 64u32;
    let h = 64u32;
    let mut pixels = Vec::with_capacity((w * h * 3) as usize);
    for _ in 0..(w * h) {
        pixels.extend_from_slice(&[0, 180, 0]);
    }
    let jpeg = encode_jpeg_rgb(w, h, &pixels);
    let src = jpeg_page_pdf(w, h, &jpeg, "", None);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 8.0, 64.0], None).unwrap();
    ed.add_redaction(0, [56.0, 0.0, 64.0, 64.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    let imgs = extracted_rgb(&out, 0);
    assert!(!imgs.is_empty(), "expected extracted image");
    let im = &imgs[0];
    let mid = im.get_pixel(32, 32).0;
    assert!(
        mid[1] > 80,
        "gap between disjoint boxes must not be a black union slab, got {mid:?}"
    );
    let left = im.get_pixel(2, 32).0;
    let right = im.get_pixel(62, 32).0;
    assert!(
        left[0] < 50 && left[1] < 50 && left[2] < 50,
        "left box must be wiped, got {left:?}"
    );
    assert!(
        right[0] < 50 && right[1] < 50 && right[2] < 50,
        "right box must be wiped, got {right:?}"
    );
}

#[test]
fn flate_rgb_burns_to_jpeg() {
    let w = 32u32;
    let h = 32u32;
    let raw = magenta_on_green(w, h);
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).unwrap();
    let flate = enc.finish().unwrap();

    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q");
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("/Type /Contents", contents.as_bytes()),
        image_xobject("FlateDecode", w, h, "DeviceRGB", 8, &flate),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "Flate secret survived");
}

#[test]
fn jpeg_inside_form_is_burned() {
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let page_c = format!("q {w} 0 0 {h} 0 0 cm /Fm1 Do Q");
    let form_c = b"q /Im1 Do Q";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 4 0 R /Resources << /XObject << /Fm1 5 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("", page_c.as_bytes()),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources << /XObject << /Im1 6 0 R >> >>",
            form_c,
        ),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("form apply");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)));
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG stream must be absent after form burn (G6)"
    );
}

#[test]
fn renamed_form_resources_are_available_to_leftover_path_walk() {
    let w = 16u32;
    let h = 16u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let page_c = b"q 1 0 0 1 0 0 cm /Fm1 Do Q q 1 0 0 1 32 0 cm /Fm1 Do Q";
    let form_c = b"q 8 0 0 8 0 0 cm /Im1 Do Q 2 12 m 14 12 l S";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 48 16] \
           /Contents 4 0 R /Resources << /XObject << /Fm1 5 0 R >> >> >>\n"
            .to_vec(),
        stream_obj("", page_c),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 16 16] \
             /Resources << /XObject << /Im1 6 0 R >> >>",
            form_c,
        ),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 8.0, 8.0], None).unwrap();
    ed.add_redaction(0, [10.0, 10.0, 16.0, 14.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("undestroyed Form path must refuse the apply");
    let msg = err.to_string();
    assert!(msg.contains("vector path"), "{msg}");
    assert!(!msg.contains("no XObject resource"), "{msg}");

    let out = save_raw(&mut ed);
    assert!(contains_bytes(&out, &jpeg), "failed apply must roll back the burned JPEG");
}

#[test]
fn form_page_scale_form_translate_maps_holes() {
    // Crate multiply is self-then-other (same as `cm`): Form /Matrix T(2,1)
    // then page S(32) → image at [64,32]–[96,64]. Swapped order would put
    // it at [2,1]–[34,33].
    let jpeg = encode_jpeg_rgb(32, 32, &magenta_on_green(32, 32));
    let page_c = b"q 32 0 0 32 0 0 cm /Fm1 Do Q";
    let form_c = b"q /Im1 Do Q";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120 80] \
           /Contents 4 0 R /Resources << /XObject << /Fm1 5 0 R >> >> >>\n"
            .to_vec(),
        stream_obj("", page_c),
        stream_obj(
            "/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Matrix [1 0 0 1 2 1] \
             /Resources << /XObject << /Im1 6 0 R >> >>",
            form_c,
        ),
        image_xobject("DCTDecode", 32, 32, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf.clone()).unwrap();
    ed.add_redaction(0, [64.0, 32.0, 96.0, 64.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("correct-order box");
    let out = save_raw(&mut ed);
    assert!(
        !has_magenta(&extracted_rgb(&out, 0)),
        "T then S must map the image onto [64,32,96,64]"
    );

    let mut ed_wrong = DocumentEditor::from_bytes(pdf).unwrap();
    ed_wrong
        .add_redaction(0, [2.0, 1.0, 34.0, 33.0], None)
        .unwrap();
    ed_wrong
        .apply_redactions_destructive(RedactionOptions::default())
        .expect("swapped-order box");
    let out_wrong = save_raw(&mut ed_wrong);
    assert!(
        has_magenta(&extracted_rgb(&out_wrong, 0)),
        "S then T rect must not wipe the secret under this crate's multiply"
    );
}

#[test]
fn flate_then_dct_filter_chain_burns() {
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&jpeg).unwrap();
    let wrapped = enc.finish().unwrap();
    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q");
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("", contents.as_bytes()),
        stream_obj(
            "/Type /XObject /Subtype /Image /Width 32 /Height 32 \
             /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter [/FlateDecode /DCTDecode]",
            &wrapped,
        ),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("flate+dct apply");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "Flate+DCT secret survived");
}

#[test]
fn two_do_same_name_only_intersecting_placement_burns() {
    let w = 16u32;
    let h = 16u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    // Two 16×16 placements on a 48×16 page with a gap so padding cannot
    // make the right placement straddle the left box.
    let contents = "q 16 0 0 16 0 0 cm /Im1 Do Q q 16 0 0 16 32 0 cm /Im1 Do Q";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 48 16] \
           /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
            .to_vec(),
        stream_obj("", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    // Only the left placement (x 0..16). Right sits at x=32..48.
    ed.add_redaction(0, [0.0, 0.0, 16.0, 16.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    let imgs = extracted_rgb(&out, 0);
    assert!(imgs.len() >= 2, "expected original + burned clone, got {}", imgs.len());
    let burned = imgs
        .iter()
        .filter(|im| {
            !im.pixels()
                .any(|p| pixel_near(&p.0, SECRET_R, SECRET_G, SECRET_B, 40))
        })
        .count();
    let intact = imgs
        .iter()
        .filter(|im| {
            im.pixels()
                .any(|p| pixel_near(&p.0, SECRET_R, SECRET_G, SECRET_B, 40))
        })
        .count();
    assert!(burned >= 1, "missing burned clone");
    assert!(intact >= 1, "original placement must keep magenta");
    assert!(
        contains_bytes(&out, &jpeg),
        "shared Do that stays original must keep the pre-burn JPEG stream"
    );
}

#[test]
fn shared_do_all_placements_burned_drops_original_stream() {
    let w = 16u32;
    let h = 16u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let contents = "q 16 0 0 16 0 0 cm /Im1 Do Q q 16 0 0 16 32 0 cm /Im1 Do Q";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 48 16] \
           /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
            .to_vec(),
        stream_obj("", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 16.0, 16.0], None).unwrap();
    ed.add_redaction(0, [32.0, 0.0, 48.0, 16.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "every extracted image must be burned");
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG must be gone when every shared Do was burned"
    );
    assert!(
        !contains_bytes(&out, b"/Im1 5 0 R"),
        "unused original XObject name must be dropped from /Resources"
    );
}

#[test]
fn two_pages_shared_image_only_redacted_page_burns() {
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let c = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q");
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 5 0 R /Resources << /XObject << /Im1 7 0 R >> >> >>\n"
        )
        .into_bytes(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] \
             /Contents 6 0 R /Resources << /XObject << /Im1 7 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("", c.as_bytes()),
        stream_obj("", c.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);

    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "page 0 still has secret");
    assert!(has_magenta(&extracted_rgb(&out, 1)), "page 1 must keep original");
    assert!(
        contains_bytes(&out, &jpeg),
        "unredacted page must keep the shared original JPEG stream"
    );
}

#[test]
fn inline_bi_intersecting_fails_apply() {
    let contents = b"q 10 0 0 10 0 0 cm BI /W 2 /H 2 /CS /DeviceRGB /BPC 8 ID \x00\xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00 EI Q";
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R /Resources << >> >>\n"
            .to_vec(),
        stream_obj("", contents),
    ]);
    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 10.0, 10.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("BI must fail");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("inline") || msg.contains("BI"),
        "unexpected error: {msg}"
    );
}

#[test]
fn unknown_font_refuses_without_image_mutate() {
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let src = jpeg_page_pdf(w, h, &jpeg, " BT /F1 12 Tf 1 0 0 1 4 4 Tm (Hi) Tj ET", None);
    let mut ed = DocumentEditor::from_bytes(src.clone()).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    assert!(ed
        .apply_redactions_destructive(RedactionOptions::default())
        .is_err());
    let out = save_raw(&mut ed);
    assert!(
        has_magenta(&extracted_rgb(&out, 0)),
        "failed apply must not persist burned images"
    );
}

#[test]
fn thumb_dropped_on_redacted_page() {
    let w = 32u32;
    let h = 32u32;
    let page_jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let mut thumb_px = vec![0u8; 8 * 8 * 3];
    // Unique cyan so we can grep the raw saved bytes.
    for px in thumb_px.chunks_mut(3) {
        px[0] = 0;
        px[1] = 255;
        px[2] = 255;
    }
    let thumb_jpeg = encode_jpeg_rgb(8, 8, &thumb_px);
    let src = jpeg_page_pdf(w, h, &page_jpeg, "", Some(&thumb_jpeg));
    assert!(
        src.windows(thumb_jpeg.len())
            .any(|w| w == thumb_jpeg.as_slice()),
        "thumb jpeg must appear in source"
    );

    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, h as f32], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .unwrap();
    let out = save_raw(&mut ed);
    assert!(
        !out.windows(thumb_jpeg.len())
            .any(|w| w == thumb_jpeg.as_slice()),
        "page /Thumb must be dropped (G6)"
    );
}

#[test]
fn jbig2_intersecting_fails_no_output_mutation() {
    let dummy = b"not-real-jbig2";
    let contents = b"q 32 0 0 32 0 0 cm /Im1 Do Q";
    let src = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 32 32] \
           /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
            .to_vec(),
        stream_obj("", contents),
        image_xobject("JBIG2Decode", 32, 32, "DeviceGray", 1, dummy),
    ]);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 32.0, 32.0], None).unwrap();
    assert!(ed
        .apply_redactions_destructive(RedactionOptions::default())
        .is_err());
    let out = save_raw(&mut ed);
    assert!(
        out.windows(dummy.len()).any(|w| w == dummy),
        "JBIG2 bytes must remain (apply failed; no burn)"
    );
}

#[test]
fn apply_overlay_under_y_flip_ctm() {
    // Leftover page Y-flip *after* the image q/Q so image CTM is unchanged.
    // Partial box: full-page [0,0,W,H] maps to itself under this flip.
    let w = 32u32;
    let h = 32u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let extra = format!(" 1 0 0 -1 0 {h} cm");
    let src = jpeg_page_pdf(w, h, &jpeg, &extra, None);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, w as f32, 12.0], None)
        .unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect("apply");
    let out = save_raw(&mut ed);
    let doc = PdfDocument::from_bytes(out).expect("reopen");
    let content = doc.get_page_content_data(0).expect("contents");
    let s = String::from_utf8_lossy(&content);
    let re_line = s
        .lines()
        .rev()
        .find(|l| l.trim_end().ends_with(" re"))
        .unwrap_or_else(|| panic!("no overlay re in: {s}"));
    let parts: Vec<&str> = re_line.split_whitespace().collect();
    assert!(parts.len() >= 5, "expected 'x y w h re', got: {re_line}");
    let y: f32 = parts[1].parse().expect("overlay y");
    let hh: f32 = parts[3].parse().expect("overlay h");
    assert!(
        y > 12.0 && y + hh <= h as f32 + 1.0,
        "overlay must be in stream space (inverse of Y-flip), got y={y} h={hh} from {re_line}"
    );
}

#[test]
fn encrypted_without_auth_fails_apply() {
    use pdf_oxide::writer::{DocumentBuilder, DocumentMetadata, PageSize};
    let mut builder =
        DocumentBuilder::new().metadata(DocumentMetadata::new().title("enc").author("test"));
    {
        let page = builder.page(PageSize::Letter);
        page.at(72.0, 720.0).text("secret").done();
    }
    let enc = builder
        .to_bytes_encrypted("userpw", "ownerpw")
        .expect("encrypt");
    let mut ed = DocumentEditor::from_bytes(enc).expect("open encrypted");
    ed.add_redaction(0, [0.0, 0.0, 100.0, 100.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("unauthenticated encrypt must fail");
    let msg = err.to_string().to_lowercase();
    assert!(msg.contains("encrypt") || msg.contains("password"), "unexpected error: {err}");
}

#[test]
fn jpeg_plus_page_fill_same_box_saves() {
    let w = 64u32;
    let h = 64u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let extra = format!(" 0 0 {w} {h} re f");
    let src = jpeg_page_pdf(w, h, &jpeg, &extra, None);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [16.0, 16.0, 48.0, 48.0], None).unwrap();
    let report = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect("white fill must not block JPEG burn");
    assert!(report.images_modified >= 1, "report = {report:?}");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "secret pixels must be burned");
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG stream must be absent from the file"
    );
}

#[test]
fn jpeg_box_and_path_title_box_same_apply_refuses() {
    let w = 64u32;
    let h = 64u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q 0 80 40 10 re f");
    let src = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} 100] \
             /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [16.0, 16.0, 48.0, 48.0], None).unwrap();
    ed.add_redaction(0, [0.0, 80.0, 40.0, 90.0], None).unwrap();
    ed.apply_redactions_destructive(RedactionOptions::default())
        .expect_err("outlined title box must fail the whole apply");
    let out = save_raw(&mut ed);
    assert!(has_magenta(&extracted_rgb(&out, 0)), "failed apply must roll back JPEG burn");
}

#[test]
fn jpeg_and_path_title_one_fat_box_saves_with_title_leftover() {
    // Accepted leftover-paint hole: one box covering the JPEG *and* the
    // outlined title path. Pixels burn; the title `re f` may remain.
    let w = 64u32;
    let h = 64u32;
    let jpeg = encode_jpeg_rgb(w, h, &magenta_on_green(w, h));
    let contents = format!("q {w} 0 0 {h} 0 0 cm /Im1 Do Q 0 80 40 10 re f");
    let src = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} 100] \
             /Contents 4 0 R /Resources << /XObject << /Im1 5 0 R >> >> >>\n"
        )
        .into_bytes(),
        stream_obj("", contents.as_bytes()),
        image_xobject("DCTDecode", w, h, "DeviceRGB", 8, &jpeg),
    ]);
    let mut ed = DocumentEditor::from_bytes(src).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 64.0, 100.0], None).unwrap();
    let report = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect("fat box that burned pixels may keep title path");
    assert!(report.images_modified >= 1, "report = {report:?}");
    let out = save_raw(&mut ed);
    assert!(!has_magenta(&extracted_rgb(&out, 0)), "secret pixels must be burned");
    assert!(
        !contains_bytes(&out, &jpeg),
        "original JPEG stream must be absent from the file"
    );
    assert!(
        out.windows(2).any(|w| w == b"80") && out.windows(2).any(|w| w == b"re"),
        "outlined title path must survive as leftover paint"
    );
}

#[test]
fn typed_glyphs_do_not_skip_inline_bi_refuse() {
    let contents = b"BT /F1 10 Tf 1 0 0 1 0 20 Tm (HI) Tj ET\nq 10 0 0 10 0 0 cm BI /W 2 /H 2 /CS /DeviceRGB /BPC 8 ID \x00\xff\x00\x00\xff\x00\x00\xff\x00\x00\xff\x00 EI Q";
    let font = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\n".to_vec();
    let pdf = assemble_pdf(&[
        b"<< /Type /Catalog /Pages 2 0 R >>\n".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>\n".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 20 30] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\n".to_vec(),
        stream_obj("", contents),
        font,
    ]);
    let mut ed = DocumentEditor::from_bytes(pdf).unwrap();
    ed.add_redaction(0, [0.0, 0.0, 20.0, 30.0], None).unwrap();
    let err = ed
        .apply_redactions_destructive(RedactionOptions::default())
        .expect_err("BI must still refuse");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("inline") || msg.contains("BI"),
        "unexpected error: {msg}"
    );
}
