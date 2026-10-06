// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_videos_handlers.go
// (videosModelBase, isXAIVideosModel, isSoraVideosModel,
// isSupportedVideosModel, canonicalXAIVideosModel, routingXAIVideosModel,
// responseVideosModel, readVideosCreateRequest, videosCreateRequestFromForm,
// firstPostForm, videoIDFromPayload, buildXAIVideosCreateRequest,
// normalizeXAIVideosSeconds, xaiVideosSizeOptions, xaiVideosAspectRatio,
// xaiVideosResolution, xaiVideosInputImageURL,
// collectXAIVideoReferenceImages, buildVideosCreateAPIResponseFromXAI,
// buildVideosFailedAPIResponse, buildVideosRetrieveAPIResponseFromXAI,
// setOpenAIVideoErrorFromXAI, markOpenAIVideoFailed,
// xaiVideoContentURLFromPayload, openAIVideoStatus) and
// openai_images_handlers.go (imagesModelParts) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI
//
// The form is read as gin-gonic/gin v1.10.1 context.go (ContentType,
// PostForm, initFormCache) and utils.go (filterFlags) (MIT) read it, through
// Go's net/http request.go (ParseMultipartForm, ParseForm, parsePostForm,
// multipartReader) (go1.26, BSD-3-Clause).
// https://github.com/gin-gonic/gin
// https://github.com/golang/go

//! What the video endpoints work out for themselves: which models they
//! take, the xAI request an OpenAI-style create becomes, and the OpenAI
//! video objects xAI's answers become.
//!
//! The OpenAI-style create takes JSON, or a form (`multipart/form-data` or
//! `application/x-www-form-urlencoded`), whose fields are read into JSON
//! first. Every model is xAI's Grok Imagine Video: `sora-2` and its
//! variants stand for `grok-imagine-video`, and
//! `grok-imagine-video-1.5-preview` is sent as `grok-imagine-video-1.5`,
//! though the credential is still picked by the alias.
//!
//! Deviations from upstream:
//! - A failed video's ID is `video_` and a version 7 UUID, without dashes;
//!   upstream's is a version 4 UUID.
//! - A form value or name that isn't valid UTF-8 is read with each bad byte
//!   as U+FFFD; Go keeps the bytes.

use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{HeaderMap, header};
use open_ferry_core::config::diff::go_url;
use open_ferry_core::multipart::{self, MAX_FORM_MEMORY};
use open_ferry_translate::go;
use uuid::Uuid;

use crate::json::{self, Val};
use crate::query;

#[cfg(test)]
mod tests;

/// The model the OpenAI-style endpoints were made for, which stands for
/// [`DEFAULT_MODEL`] (`defaultOpenAIVideosModel`).
pub(super) const SORA_MODEL: &str = "sora-2";
/// The model a request without one gets (`defaultXAIVideosModel`).
pub(super) const DEFAULT_MODEL: &str = "grok-imagine-video";
/// Grok Imagine Video 1.5 (`xaiVideos15Model`).
const MODEL_15: &str = "grok-imagine-video-1.5";
/// The alias of [`MODEL_15`] its preview had (`xaiVideos15PreviewAlias`).
const MODEL_15_PREVIEW: &str = "grok-imagine-video-1.5-preview";
/// The OpenAI-style create's path (`openAIVideosPath`).
pub(super) const OPENAI_VIDEOS_PATH: &str = "/openai/v1/videos";
/// The length of a video without `seconds` (`defaultVideosSeconds`).
const DEFAULT_SECONDS: &str = "4";
/// The size of a video without `size` (`defaultVideosSize`).
const DEFAULT_SIZE: &str = "720x1280";
/// The resolution of every size (`defaultVideosResolution`).
const DEFAULT_RESOLUTION: &str = "720p";
/// How many reference images xAI takes (`maxXAIVideoReferences`).
const MAX_REFERENCES: usize = 7;
/// The largest URL-encoded form Go reads without a body limit of its own
/// (`parsePostForm`'s `maxFormSize`): 10 MiB.
const MAX_URLENCODED_FORM: usize = 10 << 20;

/// The provider prefix and the base of `model`, both trimmed: what follows
/// the last `/`, when something does (upstream's `imagesModelParts`).
fn model_parts(model: &str) -> (&str, &str) {
    let model = model.trim();
    match model.rsplit_once('/') {
        Some((prefix, base)) if !base.is_empty() => (prefix.trim(), base.trim()),
        _ => ("", model),
    }
}

/// The base of `model`, lower case (`videosModelBase`).
fn model_base(model: &str) -> String {
    go::to_lower(model_parts(model).1)
}

/// Whether `model` is one of xAI's video models, bare or after `xai/`,
/// `x-ai/` or `grok/`, in any case (`isXAIVideosModel`).
pub(super) fn is_xai_model(model: &str) -> bool {
    let (prefix, base) = model_parts(model);
    matches!(
        go::to_lower(base).as_str(),
        DEFAULT_MODEL | MODEL_15 | MODEL_15_PREVIEW
    ) && matches!(go::to_lower(prefix).as_str(), "" | "xai" | "x-ai" | "grok")
}

/// Whether `model` is `sora-2` or one of its variants, after any prefix
/// (`isSoraVideosModel`).
fn is_sora_model(model: &str) -> bool {
    let base = model_base(model);
    base == SORA_MODEL
        || base
            .strip_prefix(SORA_MODEL)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Whether the OpenAI-style create takes `model`
/// (`isSupportedVideosModel`).
pub(super) fn is_supported_model(model: &str) -> bool {
    is_xai_model(model) || is_sora_model(model)
}

/// The model xAI is sent and the client is shown for `model`:
/// `grok-imagine-video-1.5` for it or its preview alias, else
/// `grok-imagine-video` (`canonicalXAIVideosModel`,
/// `responseVideosModel`).
pub(super) fn canonical_model(model: &str) -> &'static str {
    match model_base(model).as_str() {
        MODEL_15 | MODEL_15_PREVIEW => MODEL_15,
        _ => DEFAULT_MODEL,
    }
}

/// The model a credential is picked by for `model`: as
/// [`canonical_model`], but the preview alias stays itself
/// (`routingXAIVideosModel`).
pub(super) fn routing_model(model: &str) -> &'static str {
    match model_base(model).as_str() {
        MODEL_15 => MODEL_15,
        MODEL_15_PREVIEW => MODEL_15_PREVIEW,
        _ => DEFAULT_MODEL,
    }
}

/// The request's media type as gin's `ContentType` gives it, trimmed and
/// lower case: the first `Content-Type`, up to a space or `;`.
pub(super) fn content_type(headers: &HeaderMap) -> String {
    let value = headers
        .get(header::CONTENT_TYPE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
        .unwrap_or_default();
    let media = value.split([' ', ';']).next().unwrap_or_default();
    go::to_lower(media.trim())
}

/// Whether a create with media type `content_type` is a form
/// (`readVideosCreateRequest`).
pub(super) fn is_form(content_type: &str) -> bool {
    matches!(
        content_type,
        "multipart/form-data" | "application/x-www-form-urlencoded"
    )
}

/// The values of a form `body`, as gin's `PostForm` finds them in Go's
/// `Request.PostForm`: a URL-encoded body of at most 10 MiB, or a
/// multipart body Go's `ReadForm` reads whole. Anything else has none.
fn post_form(headers: &HeaderMap, body: &Bytes) -> Vec<(String, String)> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .map(|value| value.as_bytes())
        .unwrap_or_default();
    match multipart::parse_media_type(content_type) {
        Ok((media, params)) if media == "multipart/form-data" => {
            let Some(boundary) = params.get("boundary") else {
                return Vec::new();
            };
            let Ok(form) =
                multipart::Reader::new(body.clone(), boundary).read_form(MAX_FORM_MEMORY)
            else {
                return Vec::new();
            };
            form.values()
                .flat_map(|(name, values)| {
                    values.iter().map(move |value| {
                        (name.to_owned(), String::from_utf8_lossy(value).into_owned())
                    })
                })
                .collect()
        }
        Ok((media, _)) if media == "application/x-www-form-urlencoded" => urlencoded(body),
        Err(error) if error.media_type() == "application/x-www-form-urlencoded" => urlencoded(body),
        _ => Vec::new(),
    }
}

/// A URL-encoded form's values (`parsePostForm`): none over 10 MiB.
fn urlencoded(body: &Bytes) -> Vec<(String, String)> {
    if body.len() > MAX_URLENCODED_FORM {
        return Vec::new();
    }
    query::parse(&String::from_utf8_lossy(body))
}

/// The JSON a form create is read from (`videosCreateRequestFromForm`):
/// its trimmed fields, the input image's URL or file ID under
/// `input_reference`, and `reference_image_urls` split at commas.
pub(super) fn form_request(headers: &HeaderMap, body: &Bytes) -> Vec<u8> {
    let form = post_form(headers, body);
    let value = |name: &str| query::first(&form, name).unwrap_or_default();
    // `firstPostForm`: the first of `names` with a value.
    let first = |names: [&str; 3]| {
        names
            .into_iter()
            .map(value)
            .find(|value| !value.trim().is_empty())
            .unwrap_or_default()
    };
    let mut raw = b"{}".to_vec();
    for field in [
        "model",
        "prompt",
        "seconds",
        "size",
        "aspect_ratio",
        "resolution",
    ] {
        let value = value(field).trim();
        if !value.is_empty() {
            raw = json::set_str(&raw, field, value);
        }
    }
    let image_url = first([
        "input_reference[image_url]",
        "input_reference.image_url",
        "image_url",
    ])
    .trim();
    if !image_url.is_empty() {
        raw = json::set_str(&raw, "input_reference.image_url", image_url);
    }
    let file_id = first([
        "input_reference[file_id]",
        "input_reference.file_id",
        "file_id",
    ])
    .trim();
    if !file_id.is_empty() {
        raw = json::set_str(&raw, "input_reference.file_id", file_id);
    }
    let references: Vec<String> = value("reference_image_urls")
        .trim()
        .split(',')
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .map(json::json_string)
        .collect();
    if !references.is_empty() {
        let list = format!("[{}]", references.join(","));
        raw = json::set_raw(&raw, "reference_image_urls", list.as_bytes());
    }
    raw
}

/// The video a payload names: its `request_id`, else its `id`, trimmed
/// (`videoIDFromPayload`).
pub(super) fn video_id(payload: &[u8]) -> String {
    let id = json::str_at(payload, "request_id");
    let id = id.trim();
    if !id.is_empty() {
        return id.to_owned();
    }
    json::str_at(payload, "id").trim().to_owned()
}

/// What an OpenAI-style create asked for, for its answer
/// (`xaiVideoCreateMetadata`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CreateMeta {
    /// The model the client is shown.
    pub(super) model: &'static str,
    /// The model the credential is picked by.
    pub(super) routing_model: &'static str,
    pub(super) prompt: String,
    /// The length, in seconds, as sent.
    pub(super) seconds: String,
    pub(super) size: String,
    /// When the create was read, in Unix seconds.
    pub(super) created_at: i64,
}

/// The xAI request for an OpenAI-style create `raw` of `model`, and what
/// its answer needs, or why the create is refused
/// (`buildXAIVideosCreateRequest`).
pub(super) fn create_request(raw: &[u8], model: &str) -> Result<(Vec<u8>, CreateMeta), String> {
    let prompt = json::str_at(raw, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return Err("prompt is required".to_owned());
    }
    let (seconds, duration) = seconds(&json::str_at(raw, "seconds"))?;
    let (size, mut aspect_ratio, mut resolution) = size_options(&json::str_at(raw, "size"))?;
    if let Some(value) = aspect_ratio_of(&json::str_at(raw, "aspect_ratio")) {
        aspect_ratio = value;
    }
    if let Some(value) = resolution_of(&json::str_at(raw, "resolution")) {
        resolution = value;
    }
    let image_url = input_image_url(raw)?;
    let references = reference_images(raw);
    if references.len() > MAX_REFERENCES {
        return Err(format!(
            "reference_images supports at most {MAX_REFERENCES} images on xAI"
        ));
    }
    if !image_url.is_empty() && !references.is_empty() {
        return Err("image and reference_images cannot be combined on xAI".to_owned());
    }

    let mut request = b"{}".to_vec();
    request = json::set_str(&request, "model", canonical_model(model));
    request = json::set_str(&request, "prompt", &prompt);
    request = json::set_raw(&request, "duration", duration.to_string().as_bytes());
    request = json::set_str(&request, "aspect_ratio", aspect_ratio);
    request = json::set_str(&request, "resolution", resolution);
    if !image_url.is_empty() {
        request = json::set_str(&request, "image.url", &image_url);
    }
    if !references.is_empty() {
        let items: Vec<String> = references
            .iter()
            .map(|url| String::from_utf8_lossy(&json::set_str(b"{}", "url", url)).into_owned())
            .collect();
        let list = format!("[{}]", items.join(","));
        request = json::set_raw(&request, "reference_images", list.as_bytes());
    }
    let meta = CreateMeta {
        model: canonical_model(model),
        routing_model: routing_model(model),
        prompt,
        seconds,
        size,
        created_at: unix_now(),
    };
    Ok((request, meta))
}

/// Now, in Unix seconds.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// The video's length in seconds, as text and as a number: `raw` trimmed,
/// 4 when empty, held to 1 through 15 (`normalizeXAIVideosSeconds`).
fn seconds(raw: &str) -> Result<(String, i64), String> {
    let seconds = match raw.trim() {
        "" => DEFAULT_SECONDS,
        seconds => seconds,
    };
    let duration = seconds
        .parse::<i64>()
        .map_err(|_| "seconds must be an integer".to_owned())?
        .clamp(1, 15);
    Ok((duration.to_string(), duration))
}

/// The size, aspect ratio and resolution a `size` names
/// (`xaiVideosSizeOptions`).
fn size_options(raw: &str) -> Result<(String, &'static str, &'static str), String> {
    let size = match raw.trim() {
        "" => DEFAULT_SIZE,
        size => size,
    };
    let aspect_ratio = match size {
        "720x1280" | "1024x1792" => "9:16",
        "1280x720" | "1792x1024" => "16:9",
        _ => {
            return Err(
                "size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024".to_owned(),
            );
        }
    };
    Ok((size.to_owned(), aspect_ratio, DEFAULT_RESOLUTION))
}

/// The aspect ratio `raw` names, if any (`xaiVideosAspectRatio`).
fn aspect_ratio_of(raw: &str) -> Option<&'static str> {
    match go::to_lower(raw.trim()).as_str() {
        "1:1" | "square" => Some("1:1"),
        "16:9" | "landscape" => Some("16:9"),
        "9:16" | "portrait" => Some("9:16"),
        "4:3" => Some("4:3"),
        "3:4" => Some("3:4"),
        "3:2" => Some("3:2"),
        "2:3" => Some("2:3"),
        _ => None,
    }
}

/// The resolution `raw` names, if any (`xaiVideosResolution`).
fn resolution_of(raw: &str) -> Option<&'static str> {
    match go::to_lower(raw.trim()).as_str() {
        "480p" => Some("480p"),
        "720p" => Some("720p"),
        _ => None,
    }
}

/// The trimmed text of the value at `path` in `value`, or empty.
fn str_in(value: &Val<'_>, path: &str) -> String {
    value
        .get(path)
        .map(|found| found.str().trim().to_owned())
        .unwrap_or_default()
}

/// The URL of the image the video starts from, or empty
/// (`xaiVideosInputImageURL`): `input_reference.image_url`, else `image`
/// as a string or its `url` or `image_url.url`, else `image_url`. An
/// `input_reference` naming a file is refused.
fn input_image_url(raw: &[u8]) -> Result<String, String> {
    if let Some(input) = json::get(raw, "input_reference") {
        let image_url = str_in(&input, "image_url");
        let file_id = str_in(&input, "file_id");
        if !image_url.is_empty() && !file_id.is_empty() {
            return Err(
                "input_reference must provide exactly one of image_url or file_id".to_owned(),
            );
        }
        if !file_id.is_empty() {
            return Err("input_reference.file_id is not supported for xAI video generation; use input_reference.image_url".to_owned());
        }
        if !image_url.is_empty() {
            return Ok(image_url);
        }
    }
    if let Some(image) = json::get(raw, "image") {
        if image.is_string() {
            return Ok(image.str().trim().to_owned());
        }
        for path in ["url", "image_url.url"] {
            let url = str_in(&image, path);
            if !url.is_empty() {
                return Ok(url);
            }
        }
    }
    Ok(json::str_at(raw, "image_url").trim().to_owned())
}

/// The reference images' URLs, trimmed, from the `reference_images` then
/// the `reference_image_urls` array: each a string, or an object's `url`,
/// else its `image_url.url` (`collectXAIVideoReferenceImages`).
fn reference_images(raw: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |url: &str| {
        let url = url.trim();
        if !url.is_empty() {
            out.push(url.to_owned());
        }
    };
    for name in ["reference_images", "reference_image_urls"] {
        let Some(list) = json::get(raw, name).filter(Val::is_array) else {
            continue;
        };
        for item in list.array() {
            if item.is_string() {
                push(&item.str());
                continue;
            }
            // An object's `url` counts if it isn't empty before it is
            // trimmed, even if it is after.
            let url = item.get("url").map(|url| url.str()).unwrap_or_default();
            if !url.is_empty() {
                push(&url);
                continue;
            }
            let url = item
                .get("image_url.url")
                .map(|url| url.str())
                .unwrap_or_default();
            if !url.is_empty() {
                push(&url);
            }
        }
    }
    out
}

/// The OpenAI video object for xAI's answer `payload` to a create, or why
/// there is none (`buildVideosCreateAPIResponseFromXAI`).
pub(super) fn create_response(payload: &[u8], meta: &CreateMeta) -> Result<Vec<u8>, String> {
    let request_id = video_id(payload);
    if request_id.is_empty() {
        return Err("xAI video response did not include request_id".to_owned());
    }
    let mut out = br#"{"object":"video","progress":0,"status":"queued"}"#.to_vec();
    out = json::set_str(&out, "id", &request_id);
    out = json::set_str(&out, "model", meta.model);
    out = json::set_str(&out, "prompt", &meta.prompt);
    out = json::set_str(&out, "seconds", &meta.seconds);
    out = json::set_str(&out, "size", &meta.size);
    out = json::set_raw(&out, "created_at", meta.created_at.to_string().as_bytes());
    let status = video_status(&json::str_at(payload, "status"));
    if !status.is_empty() {
        out = json::set_str(&out, "status", status);
    }
    if let Some(progress) = json::get(payload, "progress") {
        out = json::set_raw(&out, "progress", progress.raw);
    }
    Ok(out)
}

/// A failed video object for a create that was refused
/// (`buildVideosFailedAPIResponse`): `model`, `code` and `message`
/// trimmed, each with a default when empty.
pub(super) fn failed_response(model: &str, code: &str, message: &str) -> Vec<u8> {
    let model = match model.trim() {
        "" => DEFAULT_MODEL,
        model => model,
    };
    let code = match code.trim() {
        "" => "invalid_request_error",
        code => code,
    };
    let message = match message.trim() {
        "" => "Video generation failed",
        message => message,
    };
    let id = format!("video_{}", Uuid::now_v7().simple());
    let mut out = br#"{"object":"video","status":"failed","progress":0}"#.to_vec();
    out = json::set_str(&out, "id", &id);
    out = json::set_str(&out, "model", model);
    out = json::set_str(&out, "error.code", code);
    json::set_str(&out, "error.message", message)
}

/// The OpenAI video object for xAI's answer `payload` about `video_id`
/// (`buildVideosRetrieveAPIResponseFromXAI`). Its model is the answer's,
/// else what [`canonical_model`] makes of `fallback_model`.
pub(super) fn retrieve_response(video_id: &str, payload: &[u8], fallback_model: &str) -> Vec<u8> {
    let mut out = br#"{"object":"video"}"#.to_vec();
    out = json::set_str(&out, "id", video_id);
    let model = json::str_at(payload, "model");
    let model = match model.trim() {
        "" => canonical_model(fallback_model),
        model => model,
    };
    out = json::set_str(&out, "model", model);
    for field in [
        "created_at",
        "completed_at",
        "expires_at",
        "prompt",
        "remixed_from_video_id",
        "size",
    ] {
        if let Some(value) = json::get(payload, field) {
            out = json::set_raw(&out, field, value.raw);
        }
    }
    let status = video_status(&json::str_at(payload, "status"));
    if !status.is_empty() {
        out = json::set_str(&out, "status", status);
    }
    if let Some(progress) = json::get(payload, "progress") {
        out = json::set_raw(&out, "progress", progress.raw);
    }
    if let Some(seconds) = json::get(payload, "seconds") {
        out = json::set_raw(&out, "seconds", seconds.raw);
    } else if let Some(duration) = json::get(payload, "video.duration") {
        out = json::set_str(&out, "seconds", &duration.str());
    }
    let video_url = json::str_at(payload, "video.url");
    let video_url = video_url.trim();
    if !video_url.is_empty() {
        out = json::set_str(&out, "video_url", video_url);
    }
    set_error(out, payload)
}

/// `out` with xAI's error in `payload` as an OpenAI video error
/// (`setOpenAIVideoErrorFromXAI`): an `error` object's `message`, or an
/// `error` string, with the top-level `code`, else the object's, else
/// `video_generation_failed`; or, without an `error`, the top-level `code`
/// as both. Either way the video is failed, unless it has a status.
fn set_error(mut out: Vec<u8>, payload: &[u8]) -> Vec<u8> {
    let top_code = json::str_at(payload, "code").trim().to_owned();
    let Some(error) = json::get(payload, "error") else {
        if !top_code.is_empty() {
            out = mark_failed(out);
            out = json::set_str(&out, "error.code", &top_code);
            out = json::set_str(&out, "error.message", &top_code);
        }
        return out;
    };
    out = mark_failed(out);
    let (message, own_code) = if (error.is_object() || error.is_array()) && json::valid(error.raw) {
        (str_in(&error, "message"), str_in(&error, "code"))
    } else {
        (error.str().trim().to_owned(), String::new())
    };
    if message.is_empty() {
        return out;
    }
    let code = [top_code.as_str(), own_code.as_str()]
        .into_iter()
        .find(|code| !code.is_empty())
        .unwrap_or("video_generation_failed");
    out = json::set_str(&out, "error.code", code);
    json::set_str(&out, "error.message", &message)
}

/// `out` marked failed: status `failed` and progress 0, where it has
/// neither (`markOpenAIVideoFailed`).
fn mark_failed(mut out: Vec<u8>) -> Vec<u8> {
    if json::get(&out, "status").is_none() {
        out = json::set_str(&out, "status", "failed");
    }
    if json::get(&out, "progress").is_none() {
        out = json::set_raw(&out, "progress", b"0");
    }
    out
}

/// The URL of the finished video in xAI's answer `payload`, or why there
/// is none: its trimmed `video.url`, which must be an `http` or `https` URL
/// with a host as Go's `url.Parse` reads it
/// (`xaiVideoContentURLFromPayload`).
pub(super) fn content_url(payload: &[u8]) -> Result<String, String> {
    let url = json::str_at(payload, "video.url").trim().to_owned();
    if url.is_empty() {
        return Err("xAI video response did not include video.url".to_owned());
    }
    match go_url::parse(url.as_bytes()) {
        Some(parsed)
            if matches!(parsed.scheme.as_str(), "http" | "https") && !parsed.host.is_empty() =>
        {
            Ok(url)
        }
        _ => Err("xAI video response included invalid video.url".to_owned()),
    }
}

/// The OpenAI status for xAI's `status`, or empty for one it doesn't know
/// (`openAIVideoStatus`).
pub(super) fn video_status(status: &str) -> &'static str {
    match go::to_lower(status.trim()).as_str() {
        "queued" | "pending" => "queued",
        "in_progress" | "processing" | "running" => "in_progress",
        "completed" | "done" | "succeeded" | "success" => "completed",
        "failed" | "error" | "expired" | "cancelled" | "canceled" => "failed",
        _ => "",
    }
}
