//! The ledger's queries, one or more behind each usage route.

use std::collections::{BTreeMap, HashMap};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::types::Value;
use rusqlite::{Connection, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};

/// A call's estimated cost, from its row `r` and its model's prices `p`.
const COST: &str = "((r.input_tokens - r.cache_read_tokens - r.cache_write_tokens) * p.input \
     + r.cache_read_tokens * COALESCE(p.cache_read, p.input) \
     + r.cache_write_tokens * COALESCE(p.cache_write, p.input) \
     + r.output_tokens * p.output) / 1000000.0";

/// `ms`, milliseconds since the Unix epoch, as the API writes a time.
pub(crate) fn format_time(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Which rows a query reads.
#[derive(Clone, Debug, Default)]
pub(crate) struct Filter {
    /// From this time, in milliseconds since the epoch, inclusive.
    pub(crate) from_ms: i64,
    /// To this time, exclusive.
    pub(crate) to_ms: i64,
    /// The model sent upstream.
    pub(crate) model: Option<String>,
    /// The provider.
    pub(crate) provider: Option<String>,
    /// The credential's ID; empty for the calls without one.
    pub(crate) credential: Option<String>,
    /// The client key's ID; empty for the calls without one.
    pub(crate) client_key: Option<String>,
    /// Failed calls, or the rest.
    pub(crate) failed: Option<bool>,
    /// The client request's ID.
    pub(crate) request_id: Option<String>,
}

impl Filter {
    /// The filter as SQL over `requests r`, and its parameters in order.
    fn clause(&self) -> (String, Vec<Value>) {
        let mut sql = String::from("r.ts >= ? AND r.ts < ?");
        let mut values = vec![Value::Integer(self.from_ms), Value::Integer(self.to_ms)];
        let mut text =
            |column: &str, value: &Option<String>, empty_is_null: bool| match value.as_deref() {
                None => {}
                Some("") if empty_is_null => {
                    sql.push_str(&format!(" AND {column} IS NULL"));
                }
                Some(value) => {
                    sql.push_str(&format!(" AND {column} = ?"));
                    values.push(Value::Text(value.to_owned()));
                }
            };
        text("r.model", &self.model, false);
        text("r.provider", &self.provider, false);
        text("r.credential_id", &self.credential, true);
        text("r.client_key_id", &self.client_key, true);
        text("r.request_id", &self.request_id, false);
        if let Some(failed) = self.failed {
            sql.push_str(" AND r.failed = ?");
            values.push(Value::Integer(i64::from(failed)));
        }
        (sql, values)
    }

    /// The filter, and the group's key among `keys`.
    fn clause_with_keys(
        &self,
        group: Option<GroupBy>,
        keys: Option<&[String]>,
    ) -> (String, Vec<Value>) {
        let (mut sql, mut values) = self.clause();
        if let (Some(group), Some(keys)) = (group, keys) {
            if keys.is_empty() {
                sql.push_str(" AND 0");
            } else {
                let marks = vec!["?"; keys.len()].join(", ");
                sql.push_str(&format!(" AND {} IN ({marks})", group.column()));
                values.extend(keys.iter().map(|key| Value::Text(key.clone())));
            }
        }
        (sql, values)
    }
}

/// What rows are grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupBy {
    Model,
    Provider,
    Credential,
    ClientKey,
}

impl GroupBy {
    /// The grouping named `name` in a query.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            "credential" => Some(Self::Credential),
            "client_key" => Some(Self::ClientKey),
            _ => None,
        }
    }

    /// The grouping's name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Provider => "provider",
            Self::Credential => "credential",
            Self::ClientKey => "client_key",
        }
    }

    /// The group's key over `requests r`; the calls without a credential
    /// or a client key are the group keyed `""`.
    fn column(self) -> &'static str {
        match self {
            Self::Model => "r.model",
            Self::Provider => "r.provider",
            Self::Credential => "COALESCE(r.credential_id, '')",
            Self::ClientKey => "COALESCE(r.client_key_id, '')",
        }
    }
}

/// Time buckets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bucketing {
    /// A bucket's length.
    pub(crate) size_ms: i64,
    /// The offset from UTC at which buckets start at whole multiples of
    /// their length.
    pub(crate) offset_ms: i64,
}

impl Bucketing {
    /// The start of the bucket holding `ms`.
    pub(crate) fn start(self, ms: i64) -> i64 {
        (ms + self.offset_ms).div_euclid(self.size_ms) * self.size_ms - self.offset_ms
    }

    /// The bucket's start over `requests r`, as [`Bucketing::start`] works
    /// it out for any time since 1970.
    fn column(self) -> String {
        let Self { size_ms, offset_ms } = self;
        format!("((r.ts + ({offset_ms})) / {size_ms}) * {size_ms} - ({offset_ms})")
    }
}

/// Nearest-rank percentiles, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Percentiles {
    pub(crate) p50: i64,
    pub(crate) p95: i64,
    pub(crate) p99: i64,
}

/// The sums over a set of calls.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub(crate) struct Metrics {
    pub(crate) requests: i64,
    pub(crate) errors: i64,
    pub(crate) input_tokens: i64,
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) output_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) total_tokens: i64,
    pub(crate) latency_ms: Option<Percentiles>,
    pub(crate) ttft_ms: Option<Percentiles>,
    pub(crate) cost: Option<f64>,
    pub(crate) unpriced_requests: i64,
}

/// The metrics of the rows `filter` reads, by group (`""` without one)
/// and bucket start (0 without buckets), of the groups among `keys` if
/// given.
pub(crate) fn aggregate(
    connection: &Connection,
    filter: &Filter,
    group: Option<GroupBy>,
    bucketing: Option<Bucketing>,
    keys: Option<&[String]>,
) -> rusqlite::Result<BTreeMap<(String, i64), Metrics>> {
    let g = group.map_or("''", GroupBy::column);
    let b = bucketing.map_or_else(|| "0".to_owned(), Bucketing::column);
    let (clause, values) = filter.clause_with_keys(group, keys);

    let mut metrics = BTreeMap::new();
    let sql = format!(
        "SELECT {g}, {b}, COUNT(*), SUM(r.failed), SUM(r.input_tokens), \
         SUM(r.cache_read_tokens), SUM(r.cache_write_tokens), SUM(r.output_tokens), \
         SUM(r.reasoning_tokens), SUM(r.total_tokens), SUM({COST}), \
         SUM(CASE WHEN p.model IS NULL THEN 1 ELSE 0 END) \
         FROM requests r LEFT JOIN prices p ON p.model = r.model \
         WHERE {clause} GROUP BY 1, 2"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values.iter()))?;
    while let Some(row) = rows.next()? {
        let sum = |index: usize| -> rusqlite::Result<i64> {
            Ok(row.get::<_, Option<i64>>(index)?.unwrap_or(0))
        };
        metrics.insert(
            (row.get::<_, String>(0)?, row.get::<_, i64>(1)?),
            Metrics {
                requests: sum(2)?,
                errors: sum(3)?,
                input_tokens: sum(4)?,
                cache_read_tokens: sum(5)?,
                cache_write_tokens: sum(6)?,
                output_tokens: sum(7)?,
                reasoning_tokens: sum(8)?,
                total_tokens: sum(9)?,
                latency_ms: None,
                ttft_ms: None,
                cost: row.get(10)?,
                unpriced_requests: sum(11)?,
            },
        );
    }

    for (column, ttft) in [("r.latency_ms", false), ("r.ttft_ms", true)] {
        let only = if ttft {
            " AND r.ttft_ms IS NOT NULL"
        } else {
            ""
        };
        let sql = format!(
            "SELECT g, b, \
             MAX(CASE WHEN rn = (n + 1) / 2 THEN v END), \
             MAX(CASE WHEN rn = (95 * n + 99) / 100 THEN v END), \
             MAX(CASE WHEN rn = (99 * n + 99) / 100 THEN v END) \
             FROM (SELECT g, b, v, \
                   ROW_NUMBER() OVER (PARTITION BY g, b ORDER BY v) AS rn, \
                   COUNT(*) OVER (PARTITION BY g, b) AS n \
                   FROM (SELECT {g} AS g, {b} AS b, {column} AS v \
                         FROM requests r WHERE {clause}{only})) \
             GROUP BY g, b"
        );
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values.iter()))?;
        while let Some(row) = rows.next()? {
            let key = (row.get::<_, String>(0)?, row.get::<_, i64>(1)?);
            let percentiles = Percentiles {
                p50: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                p95: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                p99: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
            };
            if let Some(entry) = metrics.get_mut(&key) {
                if ttft {
                    entry.ttft_ms = Some(percentiles);
                } else {
                    entry.latency_ms = Some(percentiles);
                }
            }
        }
    }
    Ok(metrics)
}

/// The groups of the rows `filter` reads with the most of them, most
/// first then by key, at most `limit`; and whether there were more.
pub(crate) fn top_groups(
    connection: &Connection,
    filter: &Filter,
    group: GroupBy,
    limit: usize,
) -> rusqlite::Result<(Vec<String>, bool)> {
    let (clause, mut values) = filter.clause();
    values.push(Value::Integer(
        i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1),
    ));
    let sql = format!(
        "SELECT {} AS g, COUNT(*) AS n FROM requests r WHERE {clause} \
         GROUP BY g ORDER BY n DESC, g ASC LIMIT ?",
        group.column()
    );
    let mut statement = connection.prepare(&sql)?;
    let mut keys = statement
        .query_map(params_from_iter(values.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = keys.len() > limit;
    keys.truncate(limit);
    Ok((keys, more))
}

/// A credential as the API shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct CredentialRef {
    pub(crate) id: String,
    pub(crate) auth_index: String,
    pub(crate) label: String,
    pub(crate) auth_type: String,
}

/// A client key as the API shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ClientKeyRef {
    pub(crate) id: String,
    pub(crate) masked: String,
}

/// What a group is shown as.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupInfo {
    /// What to show.
    pub(crate) label: String,
    /// A credential group's credential, `None` for the calls without one.
    pub(crate) credential: Option<CredentialRef>,
    /// A client-key group's key, `None` for the calls without one.
    pub(crate) client_key: Option<ClientKeyRef>,
}

/// How each of `keys` of `group` is shown, from its newest row that
/// `filter` reads: a model or provider by its name, a credential by its
/// label (else its ID), a client key masked.
pub(crate) fn group_info(
    connection: &Connection,
    filter: &Filter,
    group: GroupBy,
    keys: &[String],
) -> rusqlite::Result<HashMap<String, GroupInfo>> {
    let mut info: HashMap<String, GroupInfo> = keys
        .iter()
        .map(|key| {
            let label = match group {
                GroupBy::Model | GroupBy::Provider | GroupBy::Credential => key.clone(),
                GroupBy::ClientKey => String::new(),
            };
            (
                key.clone(),
                GroupInfo {
                    label,
                    ..GroupInfo::default()
                },
            )
        })
        .collect();
    if matches!(group, GroupBy::Model | GroupBy::Provider) || keys.is_empty() {
        return Ok(info);
    }
    let (clause, values) = filter.clause_with_keys(Some(group), Some(keys));
    // SQLite takes the bare columns from the row with the largest id.
    let sql = format!(
        "SELECT {} AS g, MAX(r.id), r.credential_id, r.auth_index, r.credential_label, \
         r.auth_type, r.client_key_id, r.client_key_masked \
         FROM requests r WHERE {clause} GROUP BY g",
        group.column()
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values.iter()))?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let Some(entry) = info.get_mut(&key) else {
            continue;
        };
        match group {
            GroupBy::Credential => {
                entry.credential = credential(row, 2)?;
                if let Some(credential) = &entry.credential
                    && !credential.label.is_empty()
                {
                    entry.label = credential.label.clone();
                }
            }
            GroupBy::ClientKey => {
                entry.client_key = client_key(row, 6)?;
                if let Some(client_key) = &entry.client_key {
                    entry.label = client_key.masked.clone();
                }
            }
            GroupBy::Model | GroupBy::Provider => {}
        }
    }
    Ok(info)
}

/// The credential in `row`'s four columns from `start`, if it has one.
fn credential(row: &Row<'_>, start: usize) -> rusqlite::Result<Option<CredentialRef>> {
    let Some(id) = row.get::<_, Option<String>>(start)? else {
        return Ok(None);
    };
    let text = |index: usize| -> rusqlite::Result<String> {
        Ok(row.get::<_, Option<String>>(index)?.unwrap_or_default())
    };
    Ok(Some(CredentialRef {
        id,
        auth_index: text(start + 1)?,
        label: text(start + 2)?,
        auth_type: text(start + 3)?,
    }))
}

/// The client key in `row`'s two columns from `start`, if it has one.
fn client_key(row: &Row<'_>, start: usize) -> rusqlite::Result<Option<ClientKeyRef>> {
    let Some(id) = row.get::<_, Option<String>>(start)? else {
        return Ok(None);
    };
    Ok(Some(ClientKeyRef {
        id,
        masked: row.get::<_, Option<String>>(start + 1)?.unwrap_or_default(),
    }))
}

/// Where a page of calls continues: after the row at this time and id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RequestCursor {
    t: i64,
    i: i64,
}

impl RequestCursor {
    /// The cursor as the API gives it.
    pub(crate) fn encode(self) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&self).unwrap_or_default())
    }

    /// A cursor the API gave, if `text` is one.
    pub(crate) fn decode(text: &str) -> Option<Self> {
        let bytes = URL_SAFE_NO_PAD.decode(text).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

/// A call's tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct RequestTokens {
    pub(crate) input: i64,
    pub(crate) cache_read: i64,
    pub(crate) cache_write: i64,
    pub(crate) output: i64,
    pub(crate) reasoning: i64,
    pub(crate) total: i64,
}

/// A call, as the API lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct RequestRow {
    pub(crate) id: i64,
    pub(crate) time: String,
    pub(crate) request_id: String,
    pub(crate) endpoint: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) alias: String,
    pub(crate) stream: bool,
    pub(crate) failed: bool,
    pub(crate) status: i64,
    pub(crate) latency_ms: i64,
    pub(crate) ttft_ms: Option<i64>,
    pub(crate) credential: Option<CredentialRef>,
    pub(crate) client_key: Option<ClientKeyRef>,
    pub(crate) tokens: RequestTokens,
    pub(crate) cost: Option<f64>,
}

/// The calls `filter` reads, newest first, after `cursor`, at most
/// `limit`; and the cursor of the next page, if there is one.
pub(crate) fn list_requests(
    connection: &Connection,
    filter: &Filter,
    cursor: Option<RequestCursor>,
    limit: usize,
) -> rusqlite::Result<(Vec<RequestRow>, Option<RequestCursor>)> {
    let (mut clause, mut values) = filter.clause();
    if let Some(RequestCursor { t, i }) = cursor {
        clause.push_str(" AND (r.ts < ? OR (r.ts = ? AND r.id < ?))");
        values.extend([Value::Integer(t), Value::Integer(t), Value::Integer(i)]);
    }
    values.push(Value::Integer(
        i64::try_from(limit).unwrap_or(i64::MAX).saturating_add(1),
    ));
    let sql = format!(
        "SELECT r.id, r.ts, r.request_id, r.endpoint, r.provider, r.model, r.alias, \
         r.stream, r.failed, r.status, r.latency_ms, r.ttft_ms, \
         r.credential_id, r.auth_index, r.credential_label, r.auth_type, \
         r.client_key_id, r.client_key_masked, \
         r.input_tokens, r.cache_read_tokens, r.cache_write_tokens, r.output_tokens, \
         r.reasoning_tokens, r.total_tokens, {COST} \
         FROM requests r LEFT JOIN prices p ON p.model = r.model \
         WHERE {clause} ORDER BY r.ts DESC, r.id DESC LIMIT ?"
    );
    let mut statement = connection.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(values.iter()))?;
    let mut list = Vec::new();
    let mut last = None;
    let mut more = false;
    while let Some(row) = rows.next()? {
        if list.len() == limit {
            more = true;
            break;
        }
        let id: i64 = row.get(0)?;
        let ts: i64 = row.get(1)?;
        last = Some(RequestCursor { t: ts, i: id });
        list.push(RequestRow {
            id,
            time: format_time(ts),
            request_id: row.get(2)?,
            endpoint: row.get(3)?,
            provider: row.get(4)?,
            model: row.get(5)?,
            alias: row.get(6)?,
            stream: row.get(7)?,
            failed: row.get(8)?,
            status: row.get(9)?,
            latency_ms: row.get(10)?,
            ttft_ms: row.get(11)?,
            credential: credential(row, 12)?,
            client_key: client_key(row, 16)?,
            tokens: RequestTokens {
                input: row.get(18)?,
                cache_read: row.get(19)?,
                cache_write: row.get(20)?,
                output: row.get(21)?,
                reasoning: row.get(22)?,
                total: row.get(23)?,
            },
            cost: row.get(24)?,
        });
    }
    Ok((list, if more { last } else { None }))
}

/// How many rows the ledger has, and the times of the oldest and newest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LedgerCounts {
    pub(crate) rows: i64,
    pub(crate) oldest: Option<i64>,
    pub(crate) newest: Option<i64>,
}

/// The ledger's [`LedgerCounts`].
pub(crate) fn ledger_counts(connection: &Connection) -> rusqlite::Result<LedgerCounts> {
    connection.query_row(
        "SELECT COUNT(*), MIN(ts), MAX(ts) FROM requests",
        [],
        |row| {
            Ok(LedgerCounts {
                rows: row.get(0)?,
                oldest: row.get(1)?,
                newest: row.get(2)?,
            })
        },
    )
}

/// Deletes every row, and returns how many there were.
pub(crate) fn delete_all(connection: &Connection) -> rusqlite::Result<u64> {
    let deleted = connection.execute("DELETE FROM requests", [])?;
    connection.execute_batch("PRAGMA incremental_vacuum")?;
    Ok(u64::try_from(deleted).unwrap_or(u64::MAX))
}

/// One model's prices, as the API lists them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct PriceEntry {
    pub(crate) model: String,
    pub(crate) input: f64,
    pub(crate) cache_read: Option<f64>,
    pub(crate) cache_write: Option<f64>,
    pub(crate) output: f64,
    pub(crate) updated: String,
}

/// The prices, by model.
pub(crate) fn list_prices(connection: &Connection) -> rusqlite::Result<Vec<PriceEntry>> {
    let mut statement = connection.prepare(
        "SELECT model, input, cache_read, cache_write, output, updated \
         FROM prices ORDER BY model",
    )?;
    statement
        .query_map([], price_entry)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

/// The [`PriceEntry`] in `row`.
fn price_entry(row: &Row<'_>) -> rusqlite::Result<PriceEntry> {
    Ok(PriceEntry {
        model: row.get(0)?,
        input: row.get(1)?,
        cache_read: row.get(2)?,
        cache_write: row.get(3)?,
        output: row.get(4)?,
        updated: format_time(row.get(5)?),
    })
}

/// Sets `entry`'s model's prices at `now_ms`, replacing any it had, and
/// returns them as they are listed.
pub(crate) fn upsert_price(
    connection: &Connection,
    model: &str,
    input: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    output: f64,
    now_ms: i64,
) -> rusqlite::Result<PriceEntry> {
    connection.execute(
        "INSERT INTO prices (model, input, cache_read, cache_write, output, updated) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
         ON CONFLICT (model) DO UPDATE SET input = excluded.input, \
         cache_read = excluded.cache_read, cache_write = excluded.cache_write, \
         output = excluded.output, updated = excluded.updated",
        params![model, input, cache_read, cache_write, output, now_ms],
    )?;
    connection.query_row(
        "SELECT model, input, cache_read, cache_write, output, updated \
         FROM prices WHERE model = ?1",
        params![model],
        price_entry,
    )
}

/// Removes `model`'s prices; whether it had any.
pub(crate) fn delete_price(connection: &Connection, model: &str) -> rusqlite::Result<bool> {
    Ok(connection.execute("DELETE FROM prices WHERE model = ?1", params![model])? > 0)
}

/// The models with rows and no prices, by name.
pub(crate) fn unpriced_models(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT r.model FROM requests r \
         WHERE NOT EXISTS (SELECT 1 FROM prices p WHERE p.model = r.model) \
         ORDER BY r.model",
    )?;
    statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
}
