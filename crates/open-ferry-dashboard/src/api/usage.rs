//! The usage routes, from the ledger: summary, series, requests, the
//! ledger's state and settings, deleting its rows, and prices.

use axum::body::Body;
use axum::extract::State;
use axum::response::Response;
use chrono::Utc;
use http::Uri;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{ApiError, Query, ok, read_body};
use crate::DashboardState;
use crate::ledger::{
    Bucketing, Filter, GroupBy, GroupInfo, Metrics, RequestCursor, Settings, aggregate, delete_all,
    format_time, group_info, ledger_counts, list_prices, list_requests, read_settings, top_groups,
    unpriced_models, upsert_price, write_settings,
};

/// A minute, an hour and a day, in milliseconds, by name.
const BUCKETS: [(&str, i64); 3] = [("minute", 60_000), ("hour", 3_600_000), ("day", 86_400_000)];

/// The most buckets `auto` picks.
const AUTO_BUCKETS: i64 = 200;

/// The most buckets a series has.
const MAX_BUCKETS: i64 = 1500;

/// The filters every usage route takes, and with `calls` those only the
/// list of calls does.
fn filter(query: &Query, calls: bool) -> Result<Filter, ApiError> {
    let (from_ms, to_ms) = query.range()?;
    let text = |name: &str| -> Result<Option<String>, ApiError> {
        Ok(query.non_empty(name)?.map(str::to_owned))
    };
    // An empty credential or client key is the calls without one.
    let id = |name: &str| -> Result<Option<String>, ApiError> {
        Ok(query.get(name)?.map(str::to_owned))
    };
    let mut filter = Filter {
        from_ms,
        to_ms,
        model: text("model")?,
        provider: text("provider")?,
        credential: id("credential")?,
        client_key: id("client_key")?,
        failed: None,
        request_id: None,
    };
    if calls {
        filter.failed = match query.non_empty("failed")? {
            None => None,
            Some("true") => Some(true),
            Some("false") => Some(false),
            Some(_) => return Err(ApiError::invalid("failed must be true or false")),
        };
        filter.request_id = text("request_id")?;
    }
    Ok(filter)
}

/// The `group_by` parameter.
fn group_by(query: &Query) -> Result<Option<GroupBy>, ApiError> {
    match query.non_empty("group_by")? {
        None => Ok(None),
        Some(name) => GroupBy::parse(name).map(Some).ok_or_else(|| {
            ApiError::invalid("group_by must be model, provider, credential or client_key")
        }),
    }
}

/// A group as the summary and the series show it: its key, its label and,
/// for a credential or client-key group, what it is.
fn group_fields(group: GroupBy, key: &str, info: GroupInfo) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("key".to_owned(), Value::String(key.to_owned()));
    fields.insert("label".to_owned(), Value::String(info.label));
    match group {
        GroupBy::Credential => {
            fields.insert("credential".to_owned(), json!(info.credential));
        }
        GroupBy::ClientKey => {
            fields.insert("client_key".to_owned(), json!(info.client_key));
        }
        GroupBy::Model | GroupBy::Provider => {}
    }
    fields
}

/// `GET /usage/summary`.
pub(super) async fn summary(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let filter = filter(&query, false)?;
    let group = group_by(&query)?;
    let limit = query.count("limit", 1, 500, 50)?;
    let (from, to) = (filter.from_ms, filter.to_ms);
    let (currency, totals, groups, more) = state
        .ledger
        .run(move |connection| {
            let currency = read_settings(connection)?.currency;
            let totals = aggregate(connection, &filter, None, None, None)?
                .into_values()
                .next()
                .unwrap_or_default();
            let mut groups = Vec::new();
            let mut more = false;
            if let Some(group) = group {
                let (keys, more_keys) = top_groups(connection, &filter, group, limit)?;
                more = more_keys;
                let mut metrics = aggregate(connection, &filter, Some(group), None, Some(&keys))?;
                let mut info = group_info(connection, &filter, group, &keys)?;
                for key in keys {
                    let mut fields =
                        group_fields(group, &key, info.remove(&key).unwrap_or_default());
                    let metrics = metrics.remove(&(key, 0)).unwrap_or_default();
                    fields.insert("metrics".to_owned(), json!(metrics));
                    groups.push(Value::Object(fields));
                }
            }
            Ok((currency, totals, groups, more))
        })
        .await?;
    Ok(ok(&json!({
        "from": format_time(from),
        "to": format_time(to),
        "currency": currency,
        "totals": totals,
        "group_by": group.map(GroupBy::name),
        "groups": groups,
        "more_groups": more,
    })))
}

/// `GET /usage/series`.
pub(super) async fn series(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let filter = filter(&query, false)?;
    let group = group_by(&query)?;
    let offset_minutes = query.int("utc_offset", -840, 840, 0)?;
    let series_count = query.count("groups", 1, 20, 5)?;
    let (from, to) = (filter.from_ms, filter.to_ms);
    let bucketing = |size_ms: i64| Bucketing {
        size_ms,
        offset_ms: offset_minutes * 60_000,
    };
    let buckets = |size_ms: i64| {
        let bucketing = bucketing(size_ms);
        (bucketing.start(to - 1) - bucketing.start(from)) / size_ms + 1
    };
    let (bucket, size_ms) = match query.non_empty("bucket")?.unwrap_or("auto") {
        "auto" => BUCKETS
            .iter()
            .copied()
            .find(|(_, size)| buckets(*size) <= AUTO_BUCKETS)
            .unwrap_or(("day", 86_400_000)),
        name => BUCKETS
            .iter()
            .copied()
            .find(|(bucket, _)| *bucket == name)
            .ok_or_else(|| ApiError::invalid("bucket must be minute, hour, day or auto"))?,
    };
    if buckets(size_ms) > MAX_BUCKETS {
        return Err(ApiError::invalid(format!(
            "the range has over {MAX_BUCKETS} buckets of a {bucket}; \
             take a larger bucket or a shorter range"
        )));
    }
    let bucketing = bucketing(size_ms);
    let first = bucketing.start(from);
    let last = bucketing.start(to - 1);

    let (currency, series, more) = state
        .ledger
        .run(move |connection| {
            let currency = read_settings(connection)?.currency;
            let (keys, more, mut info) = match group {
                Some(group) => {
                    let (keys, more) = top_groups(connection, &filter, group, series_count)?;
                    let info = group_info(connection, &filter, group, &keys)?;
                    (keys, more, info)
                }
                None => (vec![String::new()], false, Default::default()),
            };
            let mut metrics = aggregate(
                connection,
                &filter,
                group,
                Some(bucketing),
                group.map(|_| keys.as_slice()),
            )?;
            let mut series = Vec::new();
            for key in keys {
                let mut points = Vec::new();
                let mut start = first;
                while start <= last {
                    let metrics: Metrics =
                        metrics.remove(&(key.clone(), start)).unwrap_or_default();
                    points.push(json!({"start": format_time(start), "metrics": metrics}));
                    start += size_ms;
                }
                let mut fields = match group {
                    Some(group) => group_fields(group, &key, info.remove(&key).unwrap_or_default()),
                    None => {
                        let mut fields = Map::new();
                        fields.insert("key".to_owned(), Value::Null);
                        fields.insert("label".to_owned(), Value::Null);
                        fields
                    }
                };
                fields.insert("points".to_owned(), Value::Array(points));
                series.push(Value::Object(fields));
            }
            Ok((currency, series, more))
        })
        .await?;
    Ok(ok(&json!({
        "from": format_time(from),
        "to": format_time(to),
        "bucket": bucket,
        "bucket_seconds": size_ms / 1000,
        "currency": currency,
        "group_by": group.map(GroupBy::name),
        "series": series,
        "more_groups": more,
    })))
}

/// `GET /usage/requests`.
pub(super) async fn requests(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let filter = filter(&query, true)?;
    let limit = query.count("limit", 1, 500, 50)?;
    let cursor = match query.non_empty("cursor")? {
        None => None,
        Some(text) => Some(RequestCursor::decode(text).ok_or_else(|| {
            ApiError::new(
                http::StatusCode::BAD_REQUEST,
                "invalid_cursor",
                "the cursor isn't one this server gave",
            )
        })?),
    };
    let (rows, next) = state
        .ledger
        .run(move |connection| list_requests(connection, &filter, cursor, limit))
        .await?;
    Ok(ok(&json!({
        "requests": rows,
        "next_cursor": next.map(RequestCursor::encode),
    })))
}

/// The ledger's state, as `GET /usage/ledger` answers it.
async fn ledger_state(state: &DashboardState) -> Result<Value, ApiError> {
    let ledger = &state.ledger;
    // So that the calls below don't block.
    ledger.opened().await;
    let usage_statistics_enabled = state.management.config().usage_statistics_enabled;
    if let Some(reason) = ledger.unavailable_reason() {
        return Ok(json!({
            "available": false,
            "unavailable_reason": reason,
            "recording": false,
            "usage_statistics_enabled": usage_statistics_enabled,
            "file": null,
            "size_bytes": null,
            "rows": null,
            "oldest": null,
            "newest": null,
            "retention_days": null,
            "max_rows": null,
            "currency": null,
            "dropped_records": null,
        }));
    }
    let (settings, counts) = ledger
        .run(|connection| Ok((read_settings(connection)?, ledger_counts(connection)?)))
        .await?;
    Ok(json!({
        "available": true,
        "unavailable_reason": null,
        "recording": ledger.recording(),
        "usage_statistics_enabled": usage_statistics_enabled,
        "file": ledger.file().map(|path| path.display().to_string()),
        "size_bytes": ledger.size_bytes(),
        "rows": counts.rows,
        "oldest": counts.oldest.map(format_time),
        "newest": counts.newest.map(format_time),
        "retention_days": settings.retention_days,
        "max_rows": settings.max_rows,
        "currency": settings.currency,
        "dropped_records": ledger.dropped_records(),
    }))
}

/// `GET /usage/ledger`.
pub(super) async fn ledger(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    Ok(ok(&ledger_state(&state).await?))
}

/// The body of `PATCH /usage/ledger`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerPatch {
    retention_days: Option<i64>,
    max_rows: Option<i64>,
    currency: Option<String>,
}

/// `PATCH /usage/ledger`.
pub(super) async fn update_ledger(
    State(state): State<DashboardState>,
    body: Body,
) -> Result<Response, ApiError> {
    let patch: LedgerPatch = read_body(body).await?;
    if let Some(days) = patch.retention_days
        && !(1..=3650).contains(&days)
    {
        return Err(ApiError::invalid("retention_days must be from 1 to 3650"));
    }
    if let Some(rows) = patch.max_rows
        && !(10_000..=10_000_000).contains(&rows)
    {
        return Err(ApiError::invalid("max_rows must be from 10000 to 10000000"));
    }
    if let Some(currency) = &patch.currency
        && (!(1..=8).contains(&currency.len())
            || !currency.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return Err(ApiError::invalid("currency must be 1 to 8 letters"));
    }
    state
        .ledger
        .run(move |connection| {
            let current = read_settings(connection)?;
            let settings = Settings {
                retention_days: patch.retention_days.unwrap_or(current.retention_days),
                max_rows: patch.max_rows.unwrap_or(current.max_rows),
                currency: patch.currency.unwrap_or(current.currency),
            };
            write_settings(connection, &settings)
        })
        .await?;
    state.ledger.prune_soon();
    Ok(ok(&ledger_state(&state).await?))
}

/// `DELETE /usage/records`.
pub(super) async fn delete_records(
    State(state): State<DashboardState>,
) -> Result<Response, ApiError> {
    let deleted = state
        .ledger
        .run(|connection| delete_all(connection))
        .await?;
    Ok(ok(&json!({"deleted": deleted})))
}

/// `GET /usage/prices`.
pub(super) async fn prices(State(state): State<DashboardState>) -> Result<Response, ApiError> {
    let (currency, prices, unpriced) = state
        .ledger
        .run(|connection| {
            Ok((
                read_settings(connection)?.currency,
                list_prices(connection)?,
                unpriced_models(connection)?,
            ))
        })
        .await?;
    Ok(ok(&json!({
        "currency": currency,
        "prices": prices,
        "unpriced_models": unpriced,
    })))
}

/// The body of `PUT /usage/prices`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PriceBody {
    model: String,
    input: f64,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    output: f64,
}

/// `PUT /usage/prices`.
pub(super) async fn set_price(
    State(state): State<DashboardState>,
    body: Body,
) -> Result<Response, ApiError> {
    let price: PriceBody = read_body(body).await?;
    let length = price.model.chars().count();
    if !(1..=256).contains(&length) {
        return Err(ApiError::invalid("model must be 1 to 256 characters"));
    }
    for (name, value) in [
        ("input", Some(price.input)),
        ("cache_read", price.cache_read),
        ("cache_write", price.cache_write),
        ("output", Some(price.output)),
    ] {
        if let Some(value) = value
            && !(value.is_finite() && (0.0..=1_000_000.0).contains(&value))
        {
            return Err(ApiError::invalid(format!(
                "{name} must be a number from 0 to 1000000"
            )));
        }
    }
    let now = Utc::now().timestamp_millis();
    let entry = state
        .ledger
        .run(move |connection| {
            upsert_price(
                connection,
                &price.model,
                price.input,
                price.cache_read,
                price.cache_write,
                price.output,
                now,
            )
        })
        .await?;
    Ok(ok(&entry))
}

/// `DELETE /usage/prices?model=<model>`.
pub(super) async fn delete_price(
    State(state): State<DashboardState>,
    uri: Uri,
) -> Result<Response, ApiError> {
    let query = Query::of(&uri)?;
    let model = query
        .non_empty("model")?
        .ok_or_else(|| ApiError::invalid("model is required"))?
        .to_owned();
    let deleted = state
        .ledger
        .run(move |connection| crate::ledger::delete_price(connection, &model))
        .await?;
    Ok(ok(&json!({"deleted": deleted})))
}
