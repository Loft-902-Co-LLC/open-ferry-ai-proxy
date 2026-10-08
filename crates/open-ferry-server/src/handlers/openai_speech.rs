// Ported from CLIProxyAPI sdk/api/handlers/openai/openai_speech_handlers.go
// (AudioSpeech, XAITTS, handleXAISpeech, writeSpeechError,
// speechRoutingModel, speechModelBase, buildXAISpeechPayload,
// mapXAISpeechVoice, speechOutputFormat, speechCodecFormat,
// speechContentType, speechContentTypeNeedsDefault,
// speechResponseContentType) (v8.0.20, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! The speech endpoints, `POST /v1/audio/speech` (OpenAI's) and `POST
//! /v1/tts` (xAI's), both served by xAI's Grok TTS models and answered with
//! the audio.
//!
//! Either takes OpenAI's request or xAI's: the text is `input`, else `text`,
//! trimmed, at most 60,000 characters; the voice is `voice`, else
//! `voice_id`, OpenAI's voices mapped to Grok's (`alloy` to `ara` and so
//! on), `eve` by default; the language is `language`, `auto` by default; a
//! positive `speed` is kept; and the format is `response_format` (`mp3`,
//! `wav` or `pcm`, at 24 kHz), else xAI's `output_format` object, else MP3.
//! From these the handler writes xAI's body, which goes to the model the
//! request names: `grok-voice-tts-1.0`, or `grok-tts` for OpenAI's speech
//! models, `grok-tts` or none, a provider prefix (`xai/`, `x-ai/`, `grok/`)
//! and a thinking suffix ignored. Any other model, and a body that isn't
//! JSON or lacks text, gets a 400.
//!
//! The audio comes back with xAI's content type, unless that is missing or
//! generic (JSON, `application/octet-stream`, plain text), when it is the
//! format's (`audio/mpeg`, `audio/wav` or `audio/pcm`). There is no
//! keep-alive, as a newline before the audio would spoil it. Only these
//! endpoints may name the speech models (see [`crate::routing`]). Nothing
//! here is logged.
//!
//! Deviations from upstream:
//! - A body over the configured limit gets 413, before the 1 MB check, and
//!   a body that fails to read gets a 400 in the shape the other routes
//!   use, where upstream's starts "Invalid request:".
//! - A body that isn't UTF-8 is read as if each bad sequence were U+FFFD,
//!   so its text's length and the body written may differ from upstream's.

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use bytes::Bytes;
use http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderValue};
use open_ferry_core::exec::Format;
use open_ferry_translate::go;

use crate::body;
use crate::errors::{local_error, openai_error_response};
use crate::exec::{Call, ClientRequest};
use crate::headers::write_upstream_headers;
use crate::json;
use crate::state::AppState;

/// The model OpenAI's speech models go to (`defaultXAISpeechModel`).
const DEFAULT_MODEL: &str = "grok-tts";
/// xAI's other speech model (`xaiSpeechVoiceModel`).
const VOICE_MODEL: &str = "grok-voice-tts-1.0";
/// The voice when none is named (`defaultXAISpeechVoice`).
const DEFAULT_VOICE: &str = "eve";
/// The most characters of text (`maxXAISpeechRunes`).
const MAX_RUNES: usize = 60_000;
/// The largest body taken (`maxXAISpeechBody`).
const MAX_BODY: usize = 1 << 20;
/// The sample rate of WAV and PCM unless xAI's request names one
/// (`defaultXAISpeechSampleRate`).
const DEFAULT_SAMPLE_RATE: i64 = 24_000;
/// The provider prefixes a model may carry.
const PREFIXES: [&str; 3] = ["xai/", "x-ai/", "grok/"];

/// OpenAI's voices and the Grok voices they map to
/// (`openAISpeechVoices`). Other names are passed on.
const VOICES: [(&str, &str); 11] = [
    ("alloy", "ara"),
    ("ash", "orion"),
    ("ballad", "luna"),
    ("coral", "celeste"),
    ("echo", "rex"),
    ("fable", "sal"),
    ("onyx", "leo"),
    ("nova", "eve"),
    ("sage", "iris"),
    ("shimmer", "aurora"),
    ("verse", "lumen"),
];

/// The codec and sample rate xAI's body names, when it names any.
type OutputFormat = Option<(&'static str, i64)>;

/// `POST /v1/audio/speech` and `POST /v1/tts` (`AudioSpeech`, `XAITTS` and
/// `handleXAISpeech`).
pub(crate) async fn speech(
    State(state): State<AppState>,
    client: ClientRequest,
    body: Body,
) -> Response {
    let (passthrough, limit) = {
        let settings = state.settings();
        (
            settings.config.passthrough_headers,
            settings.config.body_limit,
        )
    };
    let raw = match body::read_decoded(&client.headers, body, limit).await {
        Ok(raw) => raw,
        Err(response) => return response,
    };
    if raw.len() > MAX_BODY {
        return speech_error("request body is larger than 1MB");
    }
    let requested = json::str_at(&raw, "model");
    let requested = requested.trim();
    let Some(model) = routing_model(requested) else {
        return speech_error(&format!(
            "Model {requested} is not supported on /v1/audio/speech. Use {DEFAULT_MODEL}."
        ));
    };
    let (payload, format) = match build_payload(&raw) {
        Ok(built) => built,
        Err(message) => return speech_error(&message),
    };

    let call = match Call::new(
        &state,
        &client,
        Format::OPENAI_SPEECH,
        model,
        Bytes::from(payload),
        "",
        false,
    ) {
        Ok(call) => call,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    let reply = match call.execute().await {
        Ok(reply) => reply,
        Err(error) => return openai_error_response(&error, passthrough),
    };
    let content_type = response_content_type(format, reply.headers.get(CONTENT_TYPE));
    let mut response = Response::new(Body::from(reply.body));
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, content_type);
    write_upstream_headers(headers, &reply.headers);
    headers.remove(CONTENT_LENGTH);
    response
}

/// A 400 for a request the handler refuses (`writeSpeechError`).
fn speech_error(message: &str) -> Response {
    local_error(400, message, "invalid_request_error")
}

/// The model a speech request goes to, if it names one served here
/// (`speechRoutingModel`).
fn routing_model(model: &str) -> Option<&'static str> {
    match model_base(model).as_str() {
        "" | "tts-1" | "tts-1-hd" | "gpt-4o-mini-tts" | DEFAULT_MODEL => Some(DEFAULT_MODEL),
        VOICE_MODEL => Some(VOICE_MODEL),
        _ => None,
    }
}

/// A model's name without a provider prefix or a thinking suffix, lower
/// case (`speechModelBase`).
fn model_base(model: &str) -> String {
    let mut model = model.trim();
    let lower = go::to_lower(model);
    if let Some(prefix) = PREFIXES.iter().find(|prefix| lower.starts_with(*prefix)) {
        // Only ASCII lowers to the prefixes' ASCII, so the original starts
        // with as many bytes.
        model = model.get(prefix.len()..).unwrap_or_default().trim();
    }
    if let Some(open) = model.rfind('(')
        && open > 0
        && model.ends_with(')')
    {
        model = model[..open].trim();
    }
    go::to_lower(model.trim())
}

/// xAI's body for a speech request, and the audio format it asks for
/// (`buildXAISpeechPayload`), or why the request is refused.
fn build_payload(raw: &[u8]) -> Result<(String, &'static str), String> {
    if !json::gjson_valid(raw) {
        return Err("body must be valid JSON".to_owned());
    }
    let mut text = json::str_at(raw, "input").trim().to_owned();
    if text.is_empty() {
        text = json::str_at(raw, "text").trim().to_owned();
    }
    if text.is_empty() {
        return Err("input is required".to_owned());
    }
    if text.chars().count() > MAX_RUNES {
        return Err(format!("input is longer than {MAX_RUNES} characters"));
    }
    let mut voice = json::str_at(raw, "voice").trim().to_owned();
    if voice.is_empty() {
        voice = json::str_at(raw, "voice_id");
    }
    let mut language = json::str_at(raw, "language").trim().to_owned();
    if language.is_empty() {
        language = "auto".to_owned();
    }
    let speed = json::get(raw, "speed")
        .filter(is_number)
        .map(|speed| speed.num())
        .filter(|speed| *speed > 0.0);
    let (format, output) = output_format(raw)?;
    if speed.is_some_and(f64::is_infinite) {
        return Err("json: unsupported value: +Inf".to_owned());
    }

    // Go's `json.Marshal` of a map: the keys sorted.
    let mut body = format!("{{\"language\":{}", json::json_string(&language));
    if let Some((codec, sample_rate)) = output {
        body.push_str(&format!(
            ",\"output_format\":{{\"codec\":{},\"sample_rate\":{sample_rate}}}",
            json::json_string(codec)
        ));
    }
    if let Some(speed) = speed {
        body.push_str(&format!(",\"speed\":{}", go::json_float(speed)));
    }
    body.push_str(&format!(
        ",\"text\":{},\"voice_id\":{}}}",
        json::json_string(&text),
        json::json_string(&map_voice(&voice))
    ));
    Ok((body, format))
}

/// Whether a value is a number (gjson's `Type == Number`).
fn is_number(value: &json::Val<'_>) -> bool {
    matches!(value.raw.first(), Some(b'-' | b'0'..=b'9'))
}

/// The Grok voice for `voice` (`mapXAISpeechVoice`).
fn map_voice(voice: &str) -> String {
    let voice = go::to_lower(voice.trim());
    if voice.is_empty() {
        return DEFAULT_VOICE.to_owned();
    }
    VOICES
        .iter()
        .find(|(openai, _)| *openai == voice)
        .map_or(voice, |(_, grok)| (*grok).to_owned())
}

/// The audio format a request asks for, and xAI's `output_format` for it
/// (`speechOutputFormat`).
fn output_format(raw: &[u8]) -> Result<(&'static str, OutputFormat), String> {
    let response_format = go::to_lower(json::str_at(raw, "response_format").trim());
    if !response_format.is_empty() {
        return codec_format(&response_format, DEFAULT_SAMPLE_RATE);
    }
    let Some(native) = json::get(raw, "output_format") else {
        return Ok(("mp3", None));
    };
    if !native.is_object() {
        return Err("output_format must be an object".to_owned());
    }
    let codec = go::to_lower(
        native
            .get("codec")
            .map(|codec| codec.str())
            .unwrap_or_default()
            .trim(),
    );
    let sample_rate = native
        .get("sample_rate")
        .filter(is_number)
        .map(|rate| rate.num())
        .filter(|rate| *rate > 0.0)
        .map_or(DEFAULT_SAMPLE_RATE, json::go_int64);
    codec_format(&codec, sample_rate)
}

/// The format and xAI's `output_format` for `codec` at `sample_rate`
/// (`speechCodecFormat`).
fn codec_format(codec: &str, sample_rate: i64) -> Result<(&'static str, OutputFormat), String> {
    let codec = match codec {
        "" | "mp3" => return Ok(("mp3", None)),
        "wav" => "wav",
        "pcm" => "pcm",
        _ => return Err("response_format must be mp3, wav, or pcm".to_owned()),
    };
    let sample_rate = if sample_rate <= 0 {
        DEFAULT_SAMPLE_RATE
    } else {
        sample_rate
    };
    Ok((codec, Some((codec, sample_rate))))
}

/// The content type of audio in `format` (`speechContentType`).
fn content_type(format: &str) -> &'static str {
    match format {
        "wav" => "audio/wav",
        "pcm" => "audio/pcm",
        _ => "audio/mpeg",
    }
}

/// Whether xAI's content type says nothing of the audio
/// (`speechContentTypeNeedsDefault`).
fn needs_default(content_type: &str) -> bool {
    let mut media_type = go::to_lower(content_type.trim());
    if let Some(at) = media_type.find(';') {
        media_type = media_type[..at].trim().to_owned();
    }
    matches!(
        media_type.as_str(),
        "" | "application/json" | "application/octet-stream" | "text/plain"
    )
}

/// The content type the audio is sent with: xAI's, unless it says nothing
/// of the audio, else the format's (`speechResponseContentType`).
fn response_content_type(format: &str, upstream: Option<&HeaderValue>) -> HeaderValue {
    match upstream {
        Some(value) if !needs_default(&String::from_utf8_lossy(value.as_bytes())) => value.clone(),
        _ => HeaderValue::from_static(content_type(format)),
    }
}

#[cfg(test)]
mod tests;
