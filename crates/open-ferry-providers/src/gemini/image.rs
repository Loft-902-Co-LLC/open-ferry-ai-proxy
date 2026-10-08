// Ported from CLIProxyAPI internal/runtime/executor/gemini_executor.go
// (fixGeminiImageAspectRatio) and internal/util/image.go
// (CreateWhiteImageBase64) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The aspect ratio of an image from `gemini-2.5-flash-image-preview`.
//!
//! That model doesn't take `generationConfig.imageConfig.aspectRatio`, so a
//! request with one and without an image of its own gets a blank white
//! picture of that shape as its first part, with a note asking the model to
//! draw over all of it, and asks for an image back. The `imageConfig` goes
//! either way.
//!
//! Deviations from upstream:
//! - The white picture is a PNG of the same size and colour type, but
//!   compressed with `flate2` rather than Go's `image/png`, so its bytes
//!   differ.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::json;

/// The model that needs this.
const IMAGE_PREVIEW_MODEL: &str = "gemini-2.5-flash-image-preview";

/// What the model is told about the blank picture.
const BLANK_IMAGE_PROMPT: &str = "Based on the following requirements, create an image within the uploaded picture. The new content *MUST* completely cover the entire area of the original picture, maintaining its exact proportions, and *NO* blank areas should appear.";

/// `fixGeminiImageAspectRatio` for a request to `model`, without its
/// thinking suffix.
pub(crate) fn fix_image_aspect_ratio(model: &str, body: &mut Value) {
    if model != IMAGE_PREVIEW_MODEL {
        return;
    }
    let Some(aspect_ratio) = json::get(body, "generationConfig.imageConfig.aspectRatio") else {
        return;
    };
    let aspect_ratio = json::str_of(Some(aspect_ratio));
    let contents = as_array(body.get("contents"));
    if !contents.is_empty() {
        let has_inline_data = contents.iter().any(|content| {
            as_array(content.get("parts"))
                .iter()
                .any(|part| part.get("inlineData").is_some())
        });
        if !has_inline_data {
            let mut parts = vec![
                json!({"text": BLANK_IMAGE_PROMPT}),
                json!({"inlineData": {"mime_type": "image/png", "data": white_png_base64(&aspect_ratio)}}),
            ];
            parts.extend(as_array(contents[0].get("parts")));
            json::set(body, "contents.0.parts", Value::Array(parts));
            json::set(
                body,
                "generationConfig.responseModalities",
                json!(["IMAGE", "TEXT"]),
            );
        }
    }
    json::delete(body, "generationConfig.imageConfig");
}

/// gjson's `Array()`: an array's items, nothing for a missing value or
/// `null`, and any other value as the only item.
fn as_array(value: Option<&Value>) -> Vec<Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    }
}

/// `CreateWhiteImageBase64`: a white PNG of the size Gemini makes for
/// `aspect_ratio`, square for one it doesn't know, in base64.
pub(crate) fn white_png_base64(aspect_ratio: &str) -> String {
    let (width, height) = match aspect_ratio {
        "2:3" => (832, 1248),
        "3:2" => (1248, 832),
        "3:4" => (864, 1184),
        "4:3" => (1184, 864),
        "4:5" => (896, 1152),
        "5:4" => (1152, 896),
        "9:16" => (768, 1344),
        "16:9" => (1344, 768),
        "21:9" => (1536, 672),
        _ => (1024, 1024),
    };
    STANDARD.encode(white_png(width, height))
}

/// An opaque white 8-bit RGB PNG of `width` by `height`.
fn white_png(width: u32, height: u32) -> Vec<u8> {
    use std::io::Write as _;

    let row = 1 + 3 * width as usize;
    let mut scanlines = vec![0xFF_u8; row * height as usize];
    for line in scanlines.chunks_mut(row) {
        // No filter.
        line[0] = 0;
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing to a Vec doesn't fail.
    let _ = encoder.write_all(&scanlines);
    let data = encoder.finish().unwrap_or_default();

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // 8 bits per sample, RGB, deflate, adaptive filtering, no interlace.
    header.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &data);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    png.extend_from_slice(&length.to_be_bytes());
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    png.extend_from_slice(&crc.finalize().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;

    /// The size, colour type and pixels of a PNG made by [`white_png`].
    fn decode(png: &[u8]) -> (u32, u32, u8, bool) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let mut at = 8;
        let mut header = Vec::new();
        let mut data = Vec::new();
        while at < png.len() {
            let length = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
            let kind = &png[at + 4..at + 8];
            let body = &png[at + 8..at + 8 + length];
            let crc =
                u32::from_be_bytes(png[at + 8 + length..at + 12 + length].try_into().unwrap());
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(kind);
            hasher.update(body);
            assert_eq!(hasher.finalize(), crc);
            match kind {
                b"IHDR" => header = body.to_vec(),
                b"IDAT" => data.extend_from_slice(body),
                _ => {}
            }
            at += 12 + length;
        }
        let width = u32::from_be_bytes(header[0..4].try_into().unwrap());
        let height = u32::from_be_bytes(header[4..8].try_into().unwrap());
        let mut pixels = Vec::new();
        flate2::read::ZlibDecoder::new(data.as_slice())
            .read_to_end(&mut pixels)
            .unwrap();
        let row = 1 + 3 * width as usize;
        let white = pixels.len() == row * height as usize
            && pixels
                .chunks(row)
                .all(|line| line[0] == 0 && line[1..].iter().all(|&b| b == 0xFF));
        (width, height, header[9], white)
    }

    #[test]
    fn white_pictures_have_the_aspect_ratio() {
        for (ratio, size) in [
            ("1:1", (1024, 1024)),
            ("2:3", (832, 1248)),
            ("16:9", (1344, 768)),
            ("21:9", (1536, 672)),
            ("7:3", (1024, 1024)),
        ] {
            let png = STANDARD.decode(white_png_base64(ratio)).unwrap();
            let (width, height, color_type, white) = decode(&png);
            assert_eq!((width, height), size, "{ratio}");
            assert_eq!(color_type, 2);
            assert!(white, "{ratio}");
        }
    }

    #[test]
    fn adds_a_blank_picture_for_the_preview_model() {
        let mut body = json!({
            "contents": [{"role": "user", "parts": [{"text": "a cat"}]}],
            "generationConfig": {"imageConfig": {"aspectRatio": "16:9"}, "temperature": 1}
        });
        fix_image_aspect_ratio(IMAGE_PREVIEW_MODEL, &mut body);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({"text": BLANK_IMAGE_PROMPT}));
        assert_eq!(parts[1]["inlineData"]["mime_type"], "image/png");
        let png = STANDARD
            .decode(parts[1]["inlineData"]["data"].as_str().unwrap())
            .unwrap();
        let (width, height, _, white) = decode(&png);
        assert_eq!((width, height, white), (1344, 768, true));
        assert_eq!(parts[2], json!({"text": "a cat"}));
        assert_eq!(
            body["generationConfig"],
            json!({"temperature": 1, "responseModalities": ["IMAGE", "TEXT"]})
        );
    }

    #[test]
    fn leaves_other_requests_alone() {
        // A request with its own image only loses the image config.
        let mut body = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "edit"}]},
                {"role": "user", "parts": [{"inlineData": {"data": "x"}}]}
            ],
            "generationConfig": {"imageConfig": {"aspectRatio": "1:1"}}
        });
        let mut want = body.clone();
        want["generationConfig"] = json!({});
        fix_image_aspect_ratio(IMAGE_PREVIEW_MODEL, &mut body);
        assert_eq!(body, want);

        // So does one without contents.
        let mut body = json!({"generationConfig": {"imageConfig": {"aspectRatio": "1:1"}}});
        fix_image_aspect_ratio(IMAGE_PREVIEW_MODEL, &mut body);
        assert_eq!(body, json!({"generationConfig": {}}));

        // Other models, and requests without an aspect ratio, keep theirs.
        let original = json!({
            "contents": [{"role": "user", "parts": [{"text": "a cat"}]}],
            "generationConfig": {"imageConfig": {"aspectRatio": "1:1"}}
        });
        let mut body = original.clone();
        fix_image_aspect_ratio("gemini-2.5-flash-image", &mut body);
        assert_eq!(body, original);
        let mut body = json!({"generationConfig": {"imageConfig": {"imageSize": "1K"}}});
        let want = body.clone();
        fix_image_aspect_ratio(IMAGE_PREVIEW_MODEL, &mut body);
        assert_eq!(body, want);
    }
}
