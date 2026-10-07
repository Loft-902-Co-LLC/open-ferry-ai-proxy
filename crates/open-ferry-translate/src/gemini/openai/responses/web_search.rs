// Ported from CLIProxyAPI internal/translator/gemini/openai/responses/gemini_openai-responses_web_search.go
// (ModelSupportsWebSearch, isResponsesWebSearchToolType, HasResponsesWebSearchTool,
// AllowsResponsesWebSearchToolChoice, ExtractResponsesWebSearchQuery,
// ExtractResponsesWebSearchAllowedDomains, ExtractGroundingMetadata,
// ExtractGroundingQueries, ExtractGroundingSources, BuildResponsesWebSearchCallItem,
// HasValidWebGrounding, MergeGroundingMetadata, MergeCitationAnnotations,
// byteOffsetToRuneOffset, GeminiPartMapping, MessageRuneRange,
// mapByteOffsetsToRuneRanges, BuildResponsesURLCitationsForMessages,
// BuildResponsesURLCitations), and the parts of internal/registry it calls
// (LookupModelInfo, AntigravityWebSearchModelFor) (v8.0.15, MIT).
// https://github.com/router-for-me/CLIProxyAPI

//! Web search for a Responses client on Gemini.
//!
//! A request's `web_search` tool becomes Gemini's `googleSearch` tool when
//! the model supports it and `tool_choice` allows it. Gemini answers with
//! grounding metadata: the queries it ran, the pages it read (chunks), and
//! which byte ranges of which text parts each page supports. That becomes a
//! `web_search_call` output item, and `url_citation` annotations on the
//! message text, with Gemini's byte offsets turned into character offsets.
//! In a stream, grounding metadata can come in pieces; they are merged, and
//! the chunk indexes later pieces use are mapped onto the merged chunks.
//!
//! Deviations from upstream:
//! - Whether a model supports web search comes from the static model
//!   catalog in use ([`ModelCatalog::current`]). Upstream asks its global
//!   registry first, which knows the models of configured accounts, and
//!   then the catalog; it also asks
//!   whether an Antigravity account has probed the model for web search
//!   (`AntigravityWebSearchModelFor`), which is never so here, since
//!   Antigravity isn't ported.
//! - Grounding chunks without a URL are told apart by their JSON written
//!   compactly; upstream compares the text Gemini sent.
//! - `HasOnlyResponsesWebSearchTools` and `mapByteOffsetsToRuneRange` aren't
//!   ported: nothing calls them.

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::{array_of, at};
use crate::json::{int_of, object, path, str_of};
use crate::models::ModelCatalog;

/// `registry.AntigravityWebSearchModelFor`: the model an Antigravity account
/// found serves web search for `model`. Antigravity isn't ported, so there
/// is never one.
fn antigravity_web_search_model_for(_model: &str) -> &'static str {
    ""
}

/// `ModelSupportsWebSearch`: whether the model has native web search. An
/// explicit `false` in the catalog wins.
pub(super) fn model_supports_web_search(model: &str) -> bool {
    let model = model.trim();
    let catalog = ModelCatalog::current();
    let Some(info) = (!model.is_empty()).then(|| catalog.lookup(model)).flatten() else {
        return !antigravity_web_search_model_for(model).is_empty();
    };
    if let Some(native) = info.native_web_search {
        return native;
    }
    !antigravity_web_search_model_for(model).is_empty() || info.supports_web_search
}

/// `isResponsesWebSearchToolType`.
pub(super) fn is_web_search_tool_type(kind: &str) -> bool {
    matches!(
        kind,
        "web_search"
            | "web_search_2025_08_26"
            | "web_search_preview"
            | "web_search_preview_2025_03_11"
    )
}

/// `HasResponsesWebSearchTool`.
pub(super) fn has_web_search_tool(root: &Value) -> bool {
    match root.get("tools") {
        Some(Value::Array(tools)) => tools
            .iter()
            .any(|tool| is_web_search_tool_type(&str_of(tool.get("type")))),
        _ => false,
    }
}

/// `AllowsResponsesWebSearchToolChoice`: whether `tool_choice` lets the
/// model search.
pub(super) fn allows_web_search_tool_choice(root: &Value) -> bool {
    match root.get("tool_choice") {
        None => true,
        Some(Value::String(choice)) => matches!(choice.as_str(), "" | "auto" | "required"),
        Some(choice @ Value::Object(_)) => match &*str_of(choice.get("type")) {
            "" | "auto" | "required" => true,
            kind if is_web_search_tool_type(kind) => true,
            "allowed_tools" => match choice.get("tools") {
                Some(Value::Array(tools)) => tools
                    .iter()
                    .any(|tool| is_web_search_tool_type(&str_of(tool.get("type")))),
                _ => false,
            },
            _ => false,
        },
        Some(_) => false,
    }
}

/// `ExtractResponsesWebSearchQuery`: what the user asked, to stand for the
/// search query when Gemini doesn't say: the input's text, the last user
/// turn's text, or else the instructions.
pub(super) fn extract_web_search_query(root: &Value) -> String {
    match root.get("input") {
        Some(Value::String(input)) => return input.trim().to_owned(),
        Some(Value::Array(items)) => {
            let mut flat_parts = Vec::new();
            let mut is_flat = true;
            for item in items {
                if str_of(item.get("type")) == "input_text" {
                    let text = str_of(item.get("text"));
                    if !text.trim().is_empty() {
                        flat_parts.push(text.trim().to_owned());
                    }
                } else if item.get("role").is_some() {
                    is_flat = false;
                    break;
                }
            }
            if is_flat && !flat_parts.is_empty() {
                return flat_parts.join("\n");
            }

            for item in items.iter().rev() {
                let role = str_of(item.get("role"));
                if !role.is_empty() && role != "user" {
                    continue;
                }
                match item.get("content") {
                    Some(Value::String(content)) if !content.trim().is_empty() => {
                        return content.trim().to_owned();
                    }
                    Some(Value::Array(parts)) => {
                        let texts: Vec<String> = parts
                            .iter()
                            .map(|part| str_of(part.get("text")).trim().to_owned())
                            .filter(|text| !text.is_empty())
                            .collect();
                        if !texts.is_empty() {
                            return texts.join("\n");
                        }
                    }
                    _ => {}
                }
                let text = str_of(item.get("text"));
                if !text.trim().is_empty() {
                    return text.trim().to_owned();
                }
            }
        }
        _ => {}
    }
    str_of(root.get("instructions")).trim().to_owned()
}

/// `ExtractResponsesWebSearchAllowedDomains`: the first web search tool's
/// `filters.allowed_domains`, without blanks.
pub(super) fn extract_allowed_domains(root: &Value) -> Vec<String> {
    let Some(Value::Array(tools)) = root.get("tools") else {
        return Vec::new();
    };
    for tool in tools {
        if !is_web_search_tool_type(&str_of(tool.get("type"))) {
            continue;
        }
        let Some(Value::Array(domains)) = path(tool, "filters.allowed_domains") else {
            continue;
        };
        return domains
            .iter()
            .map(|domain| str_of(Some(domain)).trim().to_owned())
            .filter(|domain| !domain.is_empty())
            .collect();
    }
    Vec::new()
}

/// `ExtractGroundingMetadata`: the first candidate's grounding metadata, in
/// a bare or wrapped Gemini response.
pub(super) fn extract_grounding_metadata(root: &Value) -> Option<&Value> {
    at(root, "candidates.0.groundingMetadata")
        .or_else(|| at(root, "response.candidates.0.groundingMetadata"))
}

/// `ExtractGroundingQueries`: the queries Gemini ran, without blanks.
pub(super) fn grounding_queries(metadata: Option<&Value>) -> Vec<String> {
    match metadata.and_then(|metadata| metadata.get("webSearchQueries")) {
        Some(Value::Array(queries)) => queries
            .iter()
            .map(|query| str_of(Some(query)).trim().to_owned())
            .filter(|query| !query.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// `ExtractGroundingSources`: each page Gemini read, once, as a Responses
/// source.
pub(super) fn grounding_sources(metadata: Option<&Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    for chunk in array_of(metadata.and_then(|metadata| metadata.get("groundingChunks"))) {
        let uri = chunk_uri(chunk);
        if uri.is_empty() || !seen.insert(uri.clone()) {
            continue;
        }
        sources.push(object([
            ("type", "url".into()),
            ("url", Value::String(uri)),
        ]));
    }
    sources
}

/// A grounding chunk's trimmed `web.uri`.
fn chunk_uri(chunk: &Value) -> String {
    str_of(path(chunk, "web.uri")).trim().to_owned()
}

/// A grounding chunk's trimmed `web.title`.
fn chunk_title(chunk: &Value) -> String {
    str_of(path(chunk, "web.title")).trim().to_owned()
}

/// `BuildResponsesWebSearchCallItem`.
pub(super) fn build_web_search_call_item(
    id: &str,
    query: &str,
    queries: &[String],
    sources: &[Value],
) -> Value {
    let mut action = Map::new();
    action.insert("type".to_owned(), "search".into());
    action.insert("query".to_owned(), query.into());
    if !queries.is_empty() {
        action.insert(
            "queries".to_owned(),
            queries
                .iter()
                .map(|query| Value::String(query.clone()))
                .collect(),
        );
    }
    if !sources.is_empty() {
        action.insert("sources".to_owned(), Value::Array(sources.to_vec()));
    }
    object([
        ("id", id.into()),
        ("type", "web_search_call".into()),
        ("status", "completed".into()),
        ("action", Value::Object(action)),
    ])
}

/// `HasValidWebGrounding`: whether the metadata names a query or a page.
pub(super) fn has_valid_web_grounding(metadata: Option<&Value>) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    if let Some(Value::Array(queries)) = metadata.get("webSearchQueries")
        && queries
            .iter()
            .any(|query| !str_of(Some(query)).trim().is_empty())
    {
        return true;
    }
    if let Some(Value::Array(chunks)) = metadata.get("groundingChunks")
        && chunks.iter().any(|chunk| !chunk_uri(chunk).is_empty())
    {
        return true;
    }
    false
}

/// `MergeGroundingMetadata`: `new` merged into `existing`. Queries are
/// added without repeats. Chunks are added unless one with the same URL, or
/// without a URL the same JSON, is there already; `_chunkIndexRemap` keeps,
/// across merges, which merged chunk each chunk index of the stream means,
/// and `_rawChunkCount` how many chunks the stream has sent. Supports are
/// added with their chunk indexes mapped, without repeats.
pub(super) fn merge_grounding_metadata(
    existing: Option<&Value>,
    new: Option<&Value>,
) -> Option<Value> {
    let Some(new) = new else {
        return existing.cloned();
    };
    let empty = Value::Object(Map::new());
    let existing = existing.unwrap_or(&empty);
    let mut merged = existing.clone();
    let set = |merged: &mut Value, key: &str, value: Value| {
        if let Value::Object(fields) = merged {
            fields.insert(key.to_owned(), value);
        }
    };

    // 1. Queries.
    let existing_queries = grounding_queries(Some(existing));
    let new_queries = grounding_queries(Some(new));
    if !new_queries.is_empty() {
        let mut seen = HashSet::new();
        let queries: Vec<Value> = existing_queries
            .into_iter()
            .chain(new_queries)
            .filter(|query| seen.insert(query.clone()))
            .map(Value::String)
            .collect();
        set(&mut merged, "webSearchQueries", Value::Array(queries));
    }

    // 2. Chunks, with the stream's chunk indexes mapped onto them.
    let existing_chunks = array_of(existing.get("groundingChunks"));
    let new_chunks = array_of(new.get("groundingChunks"));
    let mut remap: HashMap<i64, i64> = HashMap::new();
    if let Some(Value::Object(saved)) = existing.get("_chunkIndexRemap") {
        for (key, value) in saved {
            if let Some(index) = go_atoi(key) {
                remap.insert(index, int_of(value));
            }
        }
    }
    let previous_raw_count = match existing.get("_rawChunkCount") {
        Some(count) => int_of(count),
        None => existing_chunks.len() as i64,
    };
    if remap.is_empty() && !existing_chunks.is_empty() {
        for index in 0..existing_chunks.len() as i64 {
            remap.insert(index, index);
        }
    }

    let mut merged_chunks: Vec<Value> = existing_chunks.to_vec();
    let mut by_uri: HashMap<String, i64> = HashMap::new();
    let mut by_text: HashMap<String, i64> = HashMap::new();
    for (index, chunk) in existing_chunks.iter().enumerate() {
        let uri = chunk_uri(chunk);
        if !uri.is_empty() {
            by_uri.entry(uri).or_insert(index as i64);
        }
        by_text.insert(chunk.to_string(), index as i64);
    }
    for (index, chunk) in new_chunks.iter().enumerate() {
        let index = index as i64;
        let uri = chunk_uri(chunk);
        let title = chunk_title(chunk);
        let raw_index = previous_raw_count + index;
        let map_to = |remap: &mut HashMap<i64, i64>, target: i64| {
            remap.insert(raw_index, target);
            if previous_raw_count == 0 {
                remap.insert(index, target);
            }
        };
        let text = chunk.to_string();
        if !uri.is_empty() {
            if let Some(&existing_index) = by_uri.get(&uri) {
                map_to(&mut remap, existing_index);
                let existing_chunk = &mut merged_chunks[existing_index as usize];
                if !title.is_empty() && chunk_title(existing_chunk).is_empty() {
                    crate::json::set_path(existing_chunk, "web.title", Value::String(title));
                }
                continue;
            }
        } else if let Some(&existing_index) = by_text.get(&text) {
            map_to(&mut remap, existing_index);
            continue;
        }
        let new_index = merged_chunks.len() as i64;
        merged_chunks.push(chunk.clone());
        if !uri.is_empty() {
            by_uri.insert(uri, new_index);
        }
        by_text.insert(text, new_index);
        map_to(&mut remap, new_index);
    }
    if !merged_chunks.is_empty() {
        set(&mut merged, "groundingChunks", Value::Array(merged_chunks));
    }
    if !remap.is_empty() {
        let mut keys: Vec<i64> = remap.keys().copied().collect();
        keys.sort_unstable();
        let saved: Map<String, Value> = keys
            .into_iter()
            .map(|key| (key.to_string(), Value::from(remap[&key])))
            .collect();
        set(&mut merged, "_chunkIndexRemap", Value::Object(saved));
        set(
            &mut merged,
            "_rawChunkCount",
            Value::from(previous_raw_count + new_chunks.len() as i64),
        );
    }

    // 3. Supports, with their chunk indexes mapped.
    let existing_chunk_count = existing_chunks.len() as i64;
    let remap_indexes = |indexes: &[Value], is_existing: bool| -> (Vec<i64>, bool) {
        let mut remapped: Vec<i64> = Vec::new();
        let mut rewrite = false;
        for index in indexes {
            let old = int_of(index);
            let mut target = old;
            let should_remap =
                !is_existing || existing_chunk_count == 0 || old >= existing_chunk_count;
            if should_remap && let Some(&mapped) = remap.get(&old) {
                target = mapped;
                if target != old {
                    rewrite = true;
                }
            }
            if !remapped.contains(&target) {
                remapped.push(target);
            }
        }
        if remapped.len() != indexes.len() {
            rewrite = true;
        }
        (remapped, rewrite)
    };
    let mut seen_supports = HashSet::new();
    let mut merged_supports = Vec::new();
    let supports = array_of(existing.get("groundingSupports"))
        .iter()
        .map(|support| (support, true))
        .chain(
            array_of(new.get("groundingSupports"))
                .iter()
                .map(|support| (support, false)),
        );
    for (support, is_existing) in supports {
        let part_index = path(support, "segment.partIndex").map_or(0, int_of);
        let start = path(support, "segment.startIndex").map_or(0, int_of);
        let end = path(support, "segment.endIndex").map_or(0, int_of);
        let (indexes, rewrite) =
            remap_indexes(array_of(support.get("groundingChunkIndices")), is_existing);
        let mut sorted = indexes.clone();
        sorted.sort_unstable();
        if !seen_supports.insert((part_index, start, end, sorted)) {
            continue;
        }
        let mut support = support.clone();
        if rewrite && let Value::Object(fields) = &mut support {
            fields.insert(
                "groundingChunkIndices".to_owned(),
                indexes.into_iter().map(Value::from).collect(),
            );
        }
        merged_supports.push(support);
    }
    if !merged_supports.is_empty() {
        set(
            &mut merged,
            "groundingSupports",
            Value::Array(merged_supports),
        );
    }

    // 4 and 5. The search entry point, and the first retrieval queries.
    if let Some(entry_point) = new.get("searchEntryPoint") {
        set(&mut merged, "searchEntryPoint", entry_point.clone());
    }
    if let Some(queries) = new.get("retrievalQueries")
        && existing.get("retrievalQueries").is_none()
    {
        set(&mut merged, "retrievalQueries", queries.clone());
    }
    Some(merged)
}

/// Go's `strconv.Atoi` for an `int`: an optional sign and decimal digits.
fn go_atoi(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `MergeCitationAnnotations`: `existing` with each of `late` added, unless
/// one for the same URL and range is there; then a late one with a title
/// replaces one without.
pub(super) fn merge_citation_annotations(existing: Vec<Value>, late: Vec<Value>) -> Vec<Value> {
    if existing.is_empty() {
        return late;
    }
    if late.is_empty() {
        return existing;
    }
    let key = |annotation: &Value| {
        (
            str_of(annotation.get("url")).into_owned(),
            annotation.get("start_index").map_or(0, int_of),
            annotation.get("end_index").map_or(0, int_of),
        )
    };
    let mut result: Vec<Value> = Vec::with_capacity(existing.len() + late.len());
    let mut by_key = HashMap::new();
    for annotation in existing {
        if let std::collections::hash_map::Entry::Vacant(slot) = by_key.entry(key(&annotation)) {
            slot.insert(result.len());
            result.push(annotation);
        }
    }
    for annotation in late {
        match by_key.get(&key(&annotation)) {
            Some(&index) => {
                if str_of(result[index].get("title")).is_empty()
                    && !str_of(annotation.get("title")).is_empty()
                {
                    result[index] = annotation;
                }
            }
            None => {
                by_key.insert(key(&annotation), result.len());
                result.push(annotation);
            }
        }
    }
    result
}

/// `byteOffsetToRuneOffset`: how many characters the first `offset` bytes
/// of `text` hold, as Go counts them: a character cut off at the end counts
/// one for each of its bytes.
pub(super) fn byte_offset_to_rune_offset(text: &str, offset: i64) -> i64 {
    if offset <= 0 {
        return 0;
    }
    let bytes = text.as_bytes();
    let prefix = &bytes[..bytes
        .len()
        .min(usize::try_from(offset).unwrap_or(usize::MAX))];
    match std::str::from_utf8(prefix) {
        Ok(prefix) => prefix.chars().count() as i64,
        Err(error) => {
            let valid = error.valid_up_to();
            let whole =
                std::str::from_utf8(&prefix[..valid]).map_or(0, |text| text.chars().count());
            (whole + prefix.len() - valid) as i64
        }
    }
}

/// `GeminiPartMapping`: where a Gemini text part's text sits in a Responses
/// message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PartMapping {
    pub part_index: i64,
    pub message_index: i64,
    /// The character offset in the message where the part's text starts.
    pub start_rune: i64,
    pub text: String,
}

/// `MessageRuneRange`: a character range of one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MessageRange {
    message_index: i64,
    start: i64,
    end: i64,
}

/// `mapByteOffsetsToRuneRanges`: the message ranges a byte range of the
/// parts' text, laid end to end, covers.
fn map_byte_offsets_to_rune_ranges(
    mappings: &[PartMapping],
    start: i64,
    end: i64,
) -> Vec<MessageRange> {
    if mappings.is_empty() {
        return Vec::new();
    }
    let start = start.max(0);
    if start >= end {
        return Vec::new();
    }
    let mut spans = Vec::with_capacity(mappings.len());
    let mut total = 0i64;
    for mapping in mappings {
        let len = mapping.text.len() as i64;
        spans.push((mapping, total, total + len));
        total += len;
    }
    if start >= total {
        return Vec::new();
    }
    let end = end.min(total);

    let mut ranges: Vec<MessageRange> = Vec::new();
    for (mapping, span_start, span_end) in spans {
        let overlap_start = start.max(span_start);
        let overlap_end = end.min(span_end);
        if overlap_start >= overlap_end {
            continue;
        }
        let part_start = byte_offset_to_rune_offset(&mapping.text, overlap_start - span_start);
        let part_end = byte_offset_to_rune_offset(&mapping.text, overlap_end - span_start);
        if part_end <= part_start || part_start < 0 {
            continue;
        }
        let start_rune = mapping.start_rune + part_start;
        let end_rune = mapping.start_rune + part_end;
        match ranges.last_mut() {
            Some(last) if last.message_index == mapping.message_index && last.end == start_rune => {
                last.end = end_rune;
            }
            _ => ranges.push(MessageRange {
                message_index: mapping.message_index,
                start: start_rune,
                end: end_rune,
            }),
        }
    }
    ranges
}

/// `BuildResponsesURLCitationsForMessages`: `url_citation` annotations for
/// each message, from the grounding supports, with byte offsets in Gemini's
/// parts turned into character offsets in the messages. `None` if there are
/// no supports or no chunks.
pub(super) fn build_url_citations_for_messages(
    metadata: Option<&Value>,
    mappings: &[PartMapping],
    message_texts: &[String],
) -> Option<HashMap<i64, Vec<Value>>> {
    let metadata = metadata?;
    let chunks = array_of(metadata.get("groundingChunks"));
    let supports = array_of(metadata.get("groundingSupports"));
    if supports.is_empty() || chunks.is_empty() {
        return None;
    }

    let mut coalesced: Vec<PartMapping> = Vec::new();
    for mapping in mappings {
        match coalesced.last_mut() {
            Some(last)
                if last.part_index == mapping.part_index
                    && last.message_index == mapping.message_index =>
            {
                last.text.push_str(&mapping.text);
            }
            _ => coalesced.push(mapping.clone()),
        }
    }
    let mappings = coalesced;

    let mut result: HashMap<i64, Vec<Value>> = HashMap::new();
    let mut seen = HashSet::new();
    for support in supports {
        let segment = support.get("segment");
        let part_index_value = segment.and_then(|segment| segment.get("partIndex"));
        let part_index = part_index_value.map_or(0, int_of);
        let start = segment
            .and_then(|segment| segment.get("startIndex"))
            .map_or(0, int_of);
        let end = segment
            .and_then(|segment| segment.get("endIndex"))
            .map_or(0, int_of);

        let ranges = if part_index_value.is_some() {
            let mut part_mappings: Vec<PartMapping> = mappings
                .iter()
                .filter(|mapping| mapping.part_index == part_index)
                .cloned()
                .collect();
            if part_mappings.is_empty() && part_index == 0 && mappings.len() == 1 {
                part_mappings = mappings.clone();
            }
            map_byte_offsets_to_rune_ranges(&part_mappings, start, end)
        } else if !mappings.is_empty() {
            map_byte_offsets_to_rune_ranges(&mappings, start, end)
        } else if let Some(text) = message_texts.first() {
            let start_rune = byte_offset_to_rune_offset(text, start);
            let end_rune = byte_offset_to_rune_offset(text, end);
            if end_rune > start_rune && start_rune >= 0 {
                vec![MessageRange {
                    message_index: 0,
                    start: start_rune,
                    end: end_rune,
                }]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if ranges.is_empty() {
            continue;
        }

        for index in array_of(support.get("groundingChunkIndices")) {
            let index = int_of(index);
            let Some(chunk) = usize::try_from(index)
                .ok()
                .and_then(|index| chunks.get(index))
            else {
                continue;
            };
            let uri = chunk_uri(chunk);
            let title = chunk_title(chunk);
            if uri.is_empty() {
                continue;
            }
            for range in &ranges {
                if !seen.insert((range.message_index, uri.clone(), range.start, range.end)) {
                    continue;
                }
                result.entry(range.message_index).or_default().push(object([
                    ("type", "url_citation".into()),
                    ("url", Value::String(uri.clone())),
                    ("title", Value::String(title.clone())),
                    ("start_index", range.start.into()),
                    ("end_index", range.end.into()),
                ]));
            }
        }
    }
    Some(result)
}

/// `BuildResponsesURLCitations`: the annotations for one message, given its
/// text if there is one.
#[cfg(test)]
pub(super) fn build_url_citations(metadata: Option<&Value>, text: Option<&str>) -> Vec<Value> {
    let full_text = text.unwrap_or_default();
    let mappings = if full_text.is_empty() {
        Vec::new()
    } else {
        vec![PartMapping {
            text: full_text.to_owned(),
            ..PartMapping::default()
        }]
    };
    let texts: Vec<String> = text.map(str::to_owned).into_iter().collect();
    let Some(mut result) = build_url_citations_for_messages(metadata, &mappings, &texts) else {
        return Vec::new();
    };
    if let Some(citations) = result.remove(&0).filter(|citations| !citations.is_empty()) {
        return citations;
    }
    result
        .into_values()
        .find(|citations| !citations.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
