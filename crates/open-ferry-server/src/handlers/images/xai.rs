// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_images_handlers.go
// (isXAIImagesBaseModel, isXAIImagesModel, normalizeImagesResponseFormat,
// canonicalXAIImagesModel, xaiImagesAspectRatio, xaiImagesAspectRatioFromSize,
// xaiImagesResolution, xaiImagesRef, buildXAIImagesBaseRequest,
// buildXAIImagesGenerationsRequest, buildXAIImagesEditRequest,
// collectXAIImagesFromJSON, xaiImagesEditOptionsFromJSON,
// mimeTypeFromOutputFormat, extractXAIImagesResponse,
// buildImagesAPIResponseFromXAI and the events of streamImagesWithModel)
// (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! xAI image requests, and the OpenAI images API answers and events made
//! from xAI's answer, which an `openai-compatibility` provider's answer is
//! read as too.
//!
//! Deviations from upstream: none.

use bytes::Bytes;
use open_ferry_translate::go;

use super::model_parts;
use crate::json::{self, Val};

/// The xAI image models (upstream's `defaultXAIImagesModel`,
/// `xaiImagesQualityModel` and `xaiImages20Model`).
pub(super) const MODELS: [&str; 3] = [
    "grok-imagine-image",
    "grok-imagine-image-quality",
    "grok-imagine-image-2.0",
];

/// The aspect ratio a generation without one asks for.
const DEFAULT_ASPECT_RATIO: &str = "1:1";

/// The resolution a generation without one asks for.
const DEFAULT_RESOLUTION: &str = "1k";

/// Whether `model` is an xAI image model, alone or after `xai/`, `x-ai/` or
/// `grok/`, in any case (upstream's `isXAIImagesModel`).
pub(super) fn is_model(model: &str) -> bool {
    let (prefix, base) = model_parts(model);
    if !MODELS.contains(&go::to_lower(base.trim()).as_str()) {
        return false;
    }
    matches!(
        go::to_lower(prefix.trim()).as_str(),
        "" | "xai" | "x-ai" | "grok"
    )
}

/// `url` for `url` in any case, else `b64_json` (upstream's
/// `normalizeImagesResponseFormat`).
pub(super) fn normalize_format(format: &str) -> &'static str {
    if go::equal_fold(format.trim(), "url") {
        "url"
    } else {
        "b64_json"
    }
}

/// The xAI model `model` names: the quality or 2.0 model, else the default
/// one (upstream's `canonicalXAIImagesModel`).
pub(super) fn canonical_model(model: &str) -> &'static str {
    match super::model_base(model).as_str() {
        "grok-imagine-image-quality" => "grok-imagine-image-quality",
        "grok-imagine-image-2.0" => "grok-imagine-image-2.0",
        _ => "grok-imagine-image",
    }
}

/// The aspect ratio `raw` names, or `fallback` (upstream's
/// `xaiImagesAspectRatio`).
pub(super) fn aspect_ratio<'a>(raw: &str, fallback: &'a str) -> &'a str {
    match go::to_lower(raw.trim()).as_str() {
        "1:1" | "square" => "1:1",
        "16:9" | "landscape" => "16:9",
        "9:16" | "portrait" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => fallback,
    }
}

/// The aspect ratio an OpenAI `size` stands for, or `fallback` (upstream's
/// `xaiImagesAspectRatioFromSize`).
pub(super) fn aspect_ratio_from_size<'a>(size: &str, fallback: &'a str) -> &'a str {
    match go::to_lower(size.trim()).as_str() {
        "1024x1024" | "2048x2048" | "1:1" => "1:1",
        "1792x1024" | "16:9" => "16:9",
        "1024x1792" | "9:16" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "1536x1024" | "3:2" => "3:2",
        "1024x1536" | "2:3" => "2:3",
        _ => fallback,
    }
}

/// `1k` or `2k` when `raw` is one, `2k` for a `size` with `2048` in it, else
/// `fallback` (upstream's `xaiImagesResolution`).
pub(super) fn resolution(raw: &str, size: &str, fallback: &str) -> String {
    let raw = go::to_lower(raw.trim());
    if raw == "1k" || raw == "2k" {
        return raw;
    }
    if go::to_lower(size.trim()).contains("2048") {
        return "2k".to_owned();
    }
    fallback.to_owned()
}

/// An image reference xAI takes (upstream's `xaiImagesRef`).
fn image_ref(url: &str) -> Vec<u8> {
    json::set_str(br#"{"type":"image_url","url":""}"#, "url", url.trim())
}

/// What an xAI request asks for besides its model, prompt and format.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Options {
    /// Left out when empty.
    pub(super) aspect_ratio: String,
    /// Left out when empty.
    pub(super) resolution: String,
    /// Left out when empty once trimmed.
    pub(super) quality: String,
    /// Left out unless positive.
    pub(super) n: i64,
}

/// An xAI image request (upstream's `buildXAIImagesBaseRequest`).
fn base_request(model: &str, prompt: &str, format: &str, options: &Options) -> Vec<u8> {
    let mut request = json::set_str(b"{}", "model", canonical_model(model));
    request = json::set_str(&request, "prompt", prompt.trim());
    request = json::set_str(&request, "response_format", normalize_format(format));
    if !options.aspect_ratio.is_empty() {
        request = json::set_str(&request, "aspect_ratio", &options.aspect_ratio);
    }
    if !options.resolution.is_empty() {
        request = json::set_str(&request, "resolution", &options.resolution);
    }
    let quality = options.quality.trim();
    if !quality.is_empty() {
        request = json::set_str(&request, "quality", quality);
    }
    if options.n > 0 {
        request = json::set_raw(&request, "n", options.n.to_string().as_bytes());
    }
    request
}

/// `n` in `raw` when it is a number, else 0.
fn number_n(raw: &[u8]) -> i64 {
    json::get(raw, "n")
        .filter(|n| matches!(n.raw.first(), Some(b'-' | b'0'..=b'9')))
        .map_or(0, |n| n.int())
}

/// The xAI request for a generation in `raw` (upstream's
/// `buildXAIImagesGenerationsRequest`): its `size` read as an aspect ratio
/// over its `aspect_ratio`, 1:1 and 1k when it names neither.
pub(super) fn generations_request(raw: &[u8], model: &str, format: &str) -> Vec<u8> {
    let prompt = json::str_at(raw, "prompt");
    let size = json::str_at(raw, "size");
    let size = size.trim();
    let aspect = aspect_ratio(&json::str_at(raw, "aspect_ratio"), "");
    let aspect = match aspect_ratio_from_size(size, aspect) {
        "" => DEFAULT_ASPECT_RATIO,
        aspect => aspect,
    };
    let options = Options {
        aspect_ratio: aspect.to_owned(),
        resolution: resolution(&json::str_at(raw, "resolution"), size, DEFAULT_RESOLUTION),
        quality: json::str_at(raw, "quality").trim().to_owned(),
        n: number_n(raw),
    };
    base_request(model, prompt.trim(), format, &options)
}

/// The xAI request for an edit of `images` (upstream's
/// `buildXAIImagesEditRequest`): one image as `image`, more as `images`.
pub(super) fn edit_request(
    model: &str,
    prompt: &str,
    images: &[String],
    format: &str,
    options: &Options,
) -> Vec<u8> {
    let request = base_request(model, prompt, format, options);
    let refs: Vec<Vec<u8>> = images
        .iter()
        .map(|image| image.trim())
        .filter(|image| !image.is_empty())
        .map(image_ref)
        .collect();
    match refs.as_slice() {
        [] => request,
        [one] => json::set_raw(&request, "image", one),
        refs => json::set_raw(
            &request,
            "images",
            &[b"[".as_slice(), &refs.join(&b','), b"]"].concat(),
        ),
    }
}

/// The images of a JSON edit (upstream's `collectXAIImagesFromJSON`): an
/// `image` that is a string, or an object's `image_url.url`, `image_url`
/// and `url`; then the same of each item of `images`. Empty ones are left
/// out.
pub(super) fn images_from_json(raw: &[u8]) -> Vec<String> {
    /// An object's `image_url.url`, `image_url` when it is a string, and
    /// `url`.
    fn refs(image: &Val<'_>, push: &mut dyn FnMut(String)) {
        push(
            image
                .get("image_url.url")
                .map(|v| v.str())
                .unwrap_or_default(),
        );
        if let Some(url) = image.get("image_url").filter(Val::is_string) {
            push(url.str());
        }
        push(image.get("url").map(|v| v.str()).unwrap_or_default());
    }

    let mut images = Vec::new();
    let mut push = |url: String| {
        let url = url.trim();
        if !url.is_empty() {
            images.push(url.to_owned());
        }
    };
    if let Some(image) = json::get(raw, "image") {
        if image.is_string() {
            push(image.str());
        } else if image.is_object() || image.is_array() {
            refs(&image, &mut push);
        }
    }
    if let Some(list) = json::get(raw, "images").filter(Val::is_array) {
        for image in list.array() {
            if image.is_string() {
                push(image.str());
            } else {
                refs(&image, &mut push);
            }
        }
    }
    images
}

/// What a JSON edit asks for besides its images (upstream's
/// `xaiImagesEditOptionsFromJSON`): no aspect ratio or resolution unless
/// it names one.
pub(super) fn edit_options_from_json(raw: &[u8]) -> Options {
    let size = json::str_at(raw, "size");
    let size = size.trim();
    let aspect = aspect_ratio(&json::str_at(raw, "aspect_ratio"), "");
    Options {
        aspect_ratio: aspect_ratio_from_size(size, aspect).to_owned(),
        resolution: resolution(&json::str_at(raw, "resolution"), size, ""),
        quality: json::str_at(raw, "quality").trim().to_owned(),
        n: number_n(raw),
    }
}

/// The MIME type of an output format such as `png`, or a MIME type as it
/// is (upstream's `mimeTypeFromOutputFormat`).
pub(super) fn mime_type(format: &str) -> String {
    if format.is_empty() {
        return "image/png".to_owned();
    }
    if format.contains('/') {
        return format.to_owned();
    }
    match go::to_lower(format.trim()).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
    .to_owned()
}

/// An image in xAI's answer (upstream's `xaiImageResult`).
#[derive(Debug, PartialEq, Eq)]
struct Image {
    b64_json: String,
    url: String,
    revised_prompt: String,
    mime_type: String,
}

impl Image {
    /// The image as `format` asks for it: `url` gives its URL, or a data
    /// URL of its data; `b64_json` its data, or its URL.
    fn field(&self, format: &str) -> (&'static str, String) {
        if format == "url" {
            if self.url.is_empty() {
                let url = format!(
                    "data:{};base64,{}",
                    mime_type(&self.mime_type),
                    self.b64_json
                );
                ("url", url)
            } else {
                ("url", self.url.clone())
            }
        } else if self.b64_json.is_empty() {
            ("url", self.url.clone())
        } else {
            ("b64_json", self.b64_json.clone())
        }
    }
}

/// What xAI's answer holds.
struct Extracted<'a> {
    /// Its images, of which there is at least one.
    images: Vec<Image>,
    /// Its `created`, or now when it has none.
    created: i64,
    /// Its `usage` when that is an object.
    usage: Option<&'a [u8]>,
}

/// The images, time and usage in xAI's answer (upstream's
/// `extractXAIImagesResponse`), or why there are none.
fn extract(payload: &[u8]) -> Result<Extracted<'_>, &'static str> {
    if !json::valid(payload) {
        return Err("upstream returned invalid image response JSON");
    }
    let mut created = json::get(payload, "created").map_or(0, |v| v.int());
    if created <= 0 {
        created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
            .unwrap_or_default();
    }
    let mut images = Vec::new();
    if let Some(data) = json::get(payload, "data").filter(Val::is_array) {
        for item in data.array() {
            let field = |key: &str| item.get(key).map(|v| v.str()).unwrap_or_default();
            let mut image = Image {
                b64_json: field("b64_json").trim().to_owned(),
                url: field("url").trim().to_owned(),
                revised_prompt: field("revised_prompt").trim().to_owned(),
                mime_type: field("mime_type").trim().to_owned(),
            };
            if image.mime_type.is_empty() {
                image.mime_type = mime_type(field("output_format").trim());
            }
            if image.b64_json.is_empty() && image.url.is_empty() {
                continue;
            }
            images.push(image);
        }
    }
    if images.is_empty() {
        return Err("upstream did not return image output");
    }
    let usage = json::get(payload, "usage")
        .filter(Val::is_object)
        .map(|usage| usage.raw);
    Ok(Extracted {
        images,
        created,
        usage,
    })
}

/// The OpenAI images API answer for xAI's answer `payload`, in `format`
/// (upstream's `buildImagesAPIResponseFromXAI`), or why there is none.
pub(super) fn images_api_response(payload: &[u8], format: &str) -> Result<Bytes, &'static str> {
    let Extracted {
        images,
        created,
        usage,
    } = extract(payload)?;
    let format = normalize_format(format);
    let mut out = format!(r#"{{"created":{created},"data":["#).into_bytes();
    for (n, image) in images.iter().enumerate() {
        if n > 0 {
            out.push(b',');
        }
        let (key, value) = image.field(format);
        let mut item = json::set_str(b"{}", key, &value);
        if !image.revised_prompt.is_empty() {
            item = json::set_str(&item, "revised_prompt", &image.revised_prompt);
        }
        out.extend_from_slice(&item);
    }
    out.extend_from_slice(b"]}");
    if let Some(usage) = usage.filter(|usage| json::valid(usage)) {
        out = json::set_raw(&out, "usage", usage);
    }
    Ok(Bytes::from(out))
}

/// The `<prefix>.completed` events for xAI's answer `payload`, one an
/// image, in `format` (upstream's `streamImagesWithModel`), or why there
/// are none.
pub(super) fn completed_events(
    payload: &[u8],
    format: &str,
    prefix: &str,
) -> Result<Bytes, &'static str> {
    let Extracted { images, usage, .. } = extract(payload)?;
    let event = format!("{prefix}.completed");
    let format = normalize_format(format);
    let mut out = Vec::new();
    for image in &images {
        let mut data = json::set_str(br#"{"type":""}"#, "type", &event);
        let (key, value) = image.field(format);
        data = json::set_str(&data, key, &value);
        if let Some(usage) = usage.filter(|usage| json::valid(usage)) {
            data = json::set_raw(&data, "usage", usage);
        }
        out.extend_from_slice(format!("event: {event}\ndata: ").as_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\n\n");
    }
    Ok(Bytes::from(out))
}
