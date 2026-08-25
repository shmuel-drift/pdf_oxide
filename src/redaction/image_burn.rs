//! Pixel burn for JPEG / Flate image XObjects (destructive redaction).
//!
//! Samples in → wipe rectangle in PDF image-space → JPEG bytes out.
//! This module owns **no** document graph (no clone, rebind, or G6).
//!
//! Filter allowlist is applied **before** decode so extractors that can
//! unpack JBIG2 / JPX / CCITT cannot slip those codecs through.

use super::image_prune::ImageRedaction;
use crate::error::{Error, Result};
use crate::extractors::images::{ColorSpace, PdfImage, PixelFormat};
use crate::object::Object;
use std::collections::HashMap;

/// JPEG quality for re-encoded burned images (near-lossless; MCU pad
/// is what stops ringing from restoring secret samples).
pub const JPEG_BURN_QUALITY: u8 = 95;

/// JPEG MCU block size used for wipe expansion (ISO JPEG 8×8).
pub const JPEG_MCU: u32 = 8;

/// Maximum Form XObject recursion depth while burning.
pub const MAX_FORM_DEPTH: u32 = 32;

/// Re-encoded image ready to wrap as a new Image XObject.
#[derive(Debug, Clone)]
pub struct BurnedJpeg {
    /// JPEG bitstream (`/DCTDecode`).
    pub data: Vec<u8>,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// `true` → `/DeviceGray`, `false` → `/DeviceRGB`.
    pub gray: bool,
}

/// Filter names from an image (or Form) stream dictionary, in decode order.
pub fn filter_names(dict: &HashMap<String, Object>) -> Vec<String> {
    match dict.get("Filter") {
        Some(Object::Name(n)) => vec![n.clone()],
        Some(Object::Array(arr)) => arr
            .iter()
            .filter_map(|f| f.as_name().map(str::to_string))
            .collect(),
        None => Vec::new(),
        Some(_) => vec!["<invalid>".to_string()],
    }
}

/// `true` when every filter is `/DCTDecode` or `/FlateDecode` (name or
/// Flate↔DCT array). Empty / unknown / JBIG2 / JPX / CCITT / ASCII85 fail.
pub fn is_allowlisted_filter(names: &[String]) -> bool {
    !names.is_empty() && names.iter().all(|n| n == "DCTDecode" || n == "FlateDecode")
}

fn smask_present(dict: &HashMap<String, Object>) -> bool {
    dict.get("SMask")
        .is_some_and(|o| !matches!(o, Object::Null))
}

fn is_image_mask(dict: &HashMap<String, Object>) -> bool {
    dict.get("ImageMask").and_then(Object::as_bool) == Some(true)
}

fn color_space_is_cmyk(dict: &HashMap<String, Object>) -> bool {
    match dict.get("ColorSpace") {
        Some(Object::Name(n)) => n == "DeviceCMYK",
        Some(Object::Array(arr)) => arr
            .first()
            .and_then(Object::as_name)
            .is_some_and(|n| n == "DeviceCMYK"),
        _ => false,
    }
}

/// Fail closed unless this Image dict is JPEG/Flate, unmasked, and not CMYK.
/// Call **before** `extract_image_from_xobject`.
pub fn assert_image_burnable(dict: &HashMap<String, Object>) -> Result<()> {
    let subtype = dict.get("Subtype").and_then(Object::as_name).unwrap_or("");
    if subtype != "Image" {
        return Err(Error::Unsupported(format!(
            "redaction cannot burn XObject subtype {subtype:?}"
        )));
    }
    if is_image_mask(dict) {
        return Err(Error::Unsupported("redaction cannot burn /ImageMask images".to_string()));
    }
    if smask_present(dict) {
        return Err(Error::Unsupported("redaction cannot burn images with /SMask".to_string()));
    }
    if color_space_is_cmyk(dict) {
        return Err(Error::Unsupported("redaction cannot burn DeviceCMYK images".to_string()));
    }
    let names = filter_names(dict);
    if !is_allowlisted_filter(&names) {
        return Err(Error::Unsupported(format!(
            "redaction cannot burn image filter {names:?} (JPEG/Flate only)"
        )));
    }
    Ok(())
}

/// Map PDF image-space overwrite fractions (lower-left origin) to an
/// exclusive pixel rectangle in decoder row order (top-first), then
/// expand to JPEG MCU 8×8 alignment.
pub fn overwrite_to_mcu_rect(
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    width: u32,
    height: u32,
) -> (u32, u32, u32, u32) {
    let w = width as f32;
    let h = height as f32;
    let x0 = (u0 * w).floor().max(0.0) as u32;
    let x1 = (u1 * w).ceil().max(0.0) as u32;
    // PDF v=0 is the bottom row; decoder y=0 is the top row.
    let y0 = ((1.0 - v1) * h).floor().max(0.0) as u32;
    let y1 = ((1.0 - v0) * h).ceil().max(0.0) as u32;
    mcu_align(x0, y0, x1.min(width), y1.min(height), width, height)
}

fn mcu_align(x0: u32, y0: u32, x1: u32, y1: u32, width: u32, height: u32) -> (u32, u32, u32, u32) {
    let x0 = (x0 / JPEG_MCU) * JPEG_MCU;
    let y0 = (y0 / JPEG_MCU) * JPEG_MCU;
    let x1 = x1
        .saturating_add(JPEG_MCU - 1)
        .saturating_div(JPEG_MCU)
        .saturating_mul(JPEG_MCU)
        .min(width);
    let y1 = y1
        .saturating_add(JPEG_MCU - 1)
        .saturating_div(JPEG_MCU)
        .saturating_mul(JPEG_MCU)
        .min(height);
    (x0, y0, x1.max(x0), y1.max(y0))
}

/// AABB union of wipe decisions. **Not** the burn path — pixel burn
/// applies each overwrite separately ([`burn_image_wipes`]).
pub fn union_wipes(wipes: impl IntoIterator<Item = ImageRedaction>) -> ImageRedaction {
    let mut acc = ImageRedaction::Keep;
    for w in wipes {
        acc = match (acc, w) {
            (ImageRedaction::Keep, x) | (x, ImageRedaction::Keep) => x,
            (ImageRedaction::DeleteFull, _) | (_, ImageRedaction::DeleteFull) => {
                ImageRedaction::DeleteFull
            },
            (
                ImageRedaction::Overwrite {
                    u0: a0,
                    v0: b0,
                    u1: a1,
                    v1: b1,
                },
                ImageRedaction::Overwrite {
                    u0: c0,
                    v0: d0,
                    u1: c1,
                    v1: d1,
                },
            ) => ImageRedaction::Overwrite {
                u0: a0.min(c0),
                v0: b0.min(d0),
                u1: a1.max(c1),
                v1: b1.max(d1),
            },
        };
    }
    acc
}

fn extracted_is_cmyk(image: &PdfImage) -> bool {
    if image.color_space().components() == 4 {
        return true;
    }
    matches!(image.color_space(), ColorSpace::DeviceCMYK)
        || matches!(image.data(), crate::extractors::images::ImageData::Raw { format, .. } if *format == PixelFormat::CMYK)
}

fn samples_rgb_or_gray(image: &PdfImage) -> Result<(Vec<u8>, u32, u32, bool)> {
    if extracted_is_cmyk(image) {
        return Err(Error::Unsupported("redaction cannot burn CMYK image samples".to_string()));
    }
    let dyn_img = image.to_dynamic_image()?;
    let width = dyn_img.width();
    let height = dyn_img.height();
    if width == 0 || height == 0 {
        return Err(Error::Unsupported("redaction cannot burn a zero-sized image".to_string()));
    }
    match dyn_img {
        image::DynamicImage::ImageLuma8(buf) => Ok((buf.into_raw(), width, height, true)),
        other => {
            let rgb = other.to_rgb8();
            Ok((rgb.into_raw(), width, height, false))
        },
    }
}

fn zero_rect(pixels: &mut [u8], width: u32, channels: usize, x0: u32, y0: u32, x1: u32, y1: u32) {
    let w = width as usize;
    for y in y0..y1 {
        let row = y as usize * w * channels;
        for x in x0..x1 {
            let i = row + x as usize * channels;
            if i + channels <= pixels.len() {
                pixels[i..i + channels].fill(0);
            }
        }
    }
}

fn encode_jpeg_q95(pixels: &[u8], width: u32, height: u32, gray: bool) -> Result<Vec<u8>> {
    use image::codecs::jpeg::JpegEncoder;
    use image::ImageEncoder;

    let mut buf = Vec::new();
    let encoder = JpegEncoder::new_with_quality(&mut buf, JPEG_BURN_QUALITY);
    let color = if gray {
        image::ColorType::L8
    } else {
        image::ColorType::Rgb8
    };
    encoder
        .write_image(pixels, width, height, color.into())
        .map_err(|e| Error::Encode(format!("jpeg encode: {e}")))?;
    Ok(buf)
}

/// Wipe `redaction` (Keep is a no-op; DeleteFull is a full-rect overwrite)
/// into 8-bit RGB/Gray samples and re-encode as JPEG q95.
pub fn burn_image(image: &PdfImage, redaction: ImageRedaction) -> Result<BurnedJpeg> {
    burn_image_wipes(image, std::iter::once(redaction))
}

/// Same as [`burn_image`] with **independent** wipe decisions.
///
/// Each `Overwrite` is MCU-aligned and zeroed on its own. `DeleteFull`
/// still wipes the whole image. Do **not** AABB-union overwrites first
/// (that fills the gap between Drive marks).
pub fn burn_image_wipes(
    image: &PdfImage,
    wipes: impl IntoIterator<Item = ImageRedaction>,
) -> Result<BurnedJpeg> {
    let items: Vec<ImageRedaction> = wipes.into_iter().collect();
    if items
        .iter()
        .any(|w| matches!(w, ImageRedaction::DeleteFull))
    {
        let (mut pixels, width, height, gray) = samples_rgb_or_gray(image)?;
        let channels = if gray { 1 } else { 3 };
        zero_rect(&mut pixels, width, channels, 0, 0, width, height);
        let data = encode_jpeg_q95(&pixels, width, height, gray)?;
        return Ok(BurnedJpeg {
            data,
            width,
            height,
            gray,
        });
    }
    let overwrites: Vec<(f32, f32, f32, f32)> = items
        .into_iter()
        .filter_map(|w| match w {
            ImageRedaction::Overwrite { u0, v0, u1, v1 } => Some((u0, v0, u1, v1)),
            ImageRedaction::Keep | ImageRedaction::DeleteFull => None,
        })
        .collect();
    if overwrites.is_empty() {
        return Err(Error::InvalidOperation("burn_image called with Keep".to_string()));
    }
    let (mut pixels, width, height, gray) = samples_rgb_or_gray(image)?;
    let channels = if gray { 1 } else { 3 };
    for (u0, v0, u1, v1) in overwrites {
        let (x0, y0, x1, y1) = overwrite_to_mcu_rect(u0, v0, u1, v1, width, height);
        zero_rect(&mut pixels, width, channels, x0, y0, x1, y1);
    }
    let data = encode_jpeg_q95(&pixels, width, height, gray)?;
    Ok(BurnedJpeg {
        data,
        width,
        height,
        gray,
    })
}

/// New Image XObject: `/DCTDecode`, BPC 8, DeviceRGB/Gray, no predictor parms.
pub fn burned_xobject(burned: &BurnedJpeg) -> Object {
    let mut dict = HashMap::new();
    dict.insert("Type".to_string(), Object::Name("XObject".to_string()));
    dict.insert("Subtype".to_string(), Object::Name("Image".to_string()));
    dict.insert("Width".to_string(), Object::Integer(burned.width as i64));
    dict.insert("Height".to_string(), Object::Integer(burned.height as i64));
    dict.insert(
        "ColorSpace".to_string(),
        Object::Name(
            if burned.gray {
                "DeviceGray"
            } else {
                "DeviceRGB"
            }
            .to_string(),
        ),
    );
    dict.insert("BitsPerComponent".to_string(), Object::Integer(8));
    dict.insert("Filter".to_string(), Object::Name("DCTDecode".to_string()));
    dict.insert("Length".to_string(), Object::Integer(burned.data.len() as i64));
    Object::Stream {
        dict,
        data: bytes::Bytes::from(burned.data.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractors::images::ImageData;

    fn rgb_image(width: u32, height: u32, pixels: Vec<u8>) -> PdfImage {
        PdfImage::new(
            width,
            height,
            ColorSpace::DeviceRGB,
            8,
            ImageData::Raw {
                pixels,
                format: PixelFormat::RGB,
            },
        )
    }

    fn fill_rgb(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
        let mut p = Vec::with_capacity(width as usize * height as usize * 3);
        for _ in 0..(width * height) {
            p.extend_from_slice(&[r, g, b]);
        }
        p
    }

    fn decode_jpeg_rgb(jpeg: &[u8]) -> image::RgbImage {
        image::load_from_memory(jpeg)
            .expect("jpeg decode")
            .to_rgb8()
    }

    fn dict_with(entries: &[(&str, Object)]) -> HashMap<String, Object> {
        let mut d = HashMap::new();
        d.insert("Subtype".to_string(), Object::Name("Image".to_string()));
        d.insert("Width".to_string(), Object::Integer(8));
        d.insert("Height".to_string(), Object::Integer(8));
        d.insert("ColorSpace".to_string(), Object::Name("DeviceRGB".to_string()));
        d.insert("BitsPerComponent".to_string(), Object::Integer(8));
        for (k, v) in entries {
            d.insert((*k).to_string(), v.clone());
        }
        d
    }

    #[test]
    fn allowlist_accepts_dct_flate_and_chain() {
        for filter in [
            Object::Name("DCTDecode".to_string()),
            Object::Name("FlateDecode".to_string()),
            Object::Array(vec![
                Object::Name("FlateDecode".to_string()),
                Object::Name("DCTDecode".to_string()),
            ]),
        ] {
            let d = dict_with(&[("Filter", filter)]);
            assert_image_burnable(&d).expect("allowlisted");
        }
    }

    #[test]
    fn allowlist_rejects_jbig2_jpx_ccitt_empty_and_smask() {
        let jbig = dict_with(&[("Filter", Object::Name("JBIG2Decode".to_string()))]);
        assert!(assert_image_burnable(&jbig).is_err());

        let jpx = dict_with(&[("Filter", Object::Name("JPXDecode".to_string()))]);
        assert!(assert_image_burnable(&jpx).is_err());

        let ccitt = dict_with(&[("Filter", Object::Name("CCITTFaxDecode".to_string()))]);
        assert!(assert_image_burnable(&ccitt).is_err());

        let none = dict_with(&[]);
        assert!(assert_image_burnable(&none).is_err(), "no filter must fail");

        let mut smask = dict_with(&[("Filter", Object::Name("DCTDecode".to_string()))]);
        smask.insert("SMask".to_string(), Object::Reference(crate::object::ObjectRef::new(9, 0)));
        assert!(assert_image_burnable(&smask).is_err());

        let mut mask = dict_with(&[("Filter", Object::Name("DCTDecode".to_string()))]);
        mask.insert("ImageMask".to_string(), Object::Boolean(true));
        assert!(assert_image_burnable(&mask).is_err());

        let mut cmyk = dict_with(&[("Filter", Object::Name("DCTDecode".to_string()))]);
        cmyk.insert("ColorSpace".to_string(), Object::Name("DeviceCMYK".to_string()));
        assert!(assert_image_burnable(&cmyk).is_err());
    }

    #[test]
    fn v_flip_wipes_pdf_bottom_not_top() {
        // 16×16: top 8 decoder rows red, bottom 8 blue.
        let mut pixels = Vec::new();
        for y in 0..16u32 {
            let (r, g, b) = if y < 8 { (255, 0, 0) } else { (0, 0, 255) };
            for _ in 0..16 {
                pixels.extend_from_slice(&[r, g, b]);
            }
        }
        let img = rgb_image(16, 16, pixels);
        // PDF bottom half: v ∈ [0, 0.5] → decoder rows 8..16.
        let burned = burn_image(
            &img,
            ImageRedaction::Overwrite {
                u0: 0.0,
                v0: 0.0,
                u1: 1.0,
                v1: 0.5,
            },
        )
        .unwrap();
        let out = decode_jpeg_rgb(&burned.data);
        let top = out.get_pixel(8, 2).0;
        let bot = out.get_pixel(8, 12).0;
        assert!(top[0] > 180 && top[2] < 80, "top (PDF v≈1) must stay red, got {top:?}");
        assert!(
            bot[0] < 40 && bot[1] < 40 && bot[2] < 40,
            "PDF-bottom (decoder bottom) must be wiped, got {bot:?}"
        );
    }

    #[test]
    fn mcu_pad_destroys_full_8x8_block() {
        let mut pixels = fill_rgb(16, 16, 0, 255, 0);
        // Distinct secret at decoder (1,1) — not MCU-aligned.
        let i = (1 * 16 + 1) * 3;
        pixels[i] = 255;
        pixels[i + 1] = 0;
        pixels[i + 2] = 255;
        let img = rgb_image(16, 16, pixels);
        let burned = burn_image(
            &img,
            ImageRedaction::Overwrite {
                u0: 1.0 / 16.0,
                v0: 1.0 - 2.0 / 16.0,
                u1: 2.0 / 16.0,
                v1: 1.0 - 1.0 / 16.0,
            },
        )
        .unwrap();
        let out = decode_jpeg_rgb(&burned.data);
        for y in 0..8 {
            for x in 0..8 {
                let p = out.get_pixel(x, y).0;
                assert!(
                    p[0] < 40 && p[1] < 40 && p[2] < 40,
                    "MCU block ({x},{y}) must be destroyed, got {p:?}"
                );
            }
        }
        let outside = out.get_pixel(12, 12).0;
        assert!(outside[1] > 180, "outside MCU pad must remain green, got {outside:?}");
    }

    #[test]
    fn disjoint_overwrites_leave_the_gap() {
        let img = rgb_image(32, 32, fill_rgb(32, 32, 0, 255, 0));
        let burned = burn_image_wipes(
            &img,
            [
                ImageRedaction::Overwrite {
                    u0: 0.0,
                    v0: 0.0,
                    u1: 0.25,
                    v1: 1.0,
                },
                ImageRedaction::Overwrite {
                    u0: 0.75,
                    v0: 0.0,
                    u1: 1.0,
                    v1: 1.0,
                },
            ],
        )
        .unwrap();
        let out = decode_jpeg_rgb(&burned.data);
        let mid = out.get_pixel(16, 16).0;
        assert!(mid[1] > 180, "gap between disjoint wipes must stay green, got {mid:?}");
        let left = out.get_pixel(2, 16).0;
        let right = out.get_pixel(30, 16).0;
        assert!(
            left[0] < 40 && left[1] < 40 && left[2] < 40,
            "left strip must be wiped, got {left:?}"
        );
        assert!(
            right[0] < 40 && right[1] < 40 && right[2] < 40,
            "right strip must be wiped, got {right:?}"
        );
    }

    #[test]
    fn flate_raw_rgb_burns_to_dctdecode_dict() {
        let img = rgb_image(16, 16, fill_rgb(16, 16, 10, 20, 30));
        let burned = burn_image(&img, ImageRedaction::DeleteFull).unwrap();
        assert!(!burned.gray);
        let obj = burned_xobject(&burned);
        let dict = obj.as_dict().unwrap();
        assert_eq!(dict.get("Filter").and_then(Object::as_name), Some("DCTDecode"));
        assert_eq!(dict.get("ColorSpace").and_then(Object::as_name), Some("DeviceRGB"));
        assert_eq!(dict.get("BitsPerComponent").and_then(Object::as_integer), Some(8));
        assert!(!dict.contains_key("DecodeParms"));
        let out = decode_jpeg_rgb(&burned.data);
        let p = out.get_pixel(4, 4).0;
        assert!(p[0] < 40 && p[1] < 40 && p[2] < 40, "full wipe, got {p:?}");
    }

    #[test]
    fn delete_full_is_full_rect_overwrite() {
        let img = rgb_image(8, 8, fill_rgb(8, 8, 255, 255, 255));
        let burned = burn_image(&img, ImageRedaction::DeleteFull).unwrap();
        let out = decode_jpeg_rgb(&burned.data);
        let p = out.get_pixel(0, 0).0;
        assert!(p[0] < 40 && p[1] < 40 && p[2] < 40);
    }
}
