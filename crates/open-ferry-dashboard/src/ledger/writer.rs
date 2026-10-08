//! The ledger's writer thread: it writes the usage records it gets into
//! rows, many to a transaction, and prunes the rows the settings don't
//! keep.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use chrono::Utc;
use open_ferry_core::observe::usage::UsageEvent;
use rusqlite::{Connection, params};

use super::Shared;
use super::schema::{client_key_secret, read_settings};
use crate::hmac::hmac_sha256;

/// The most records written in one transaction.
const BATCH: usize = 500;

/// How long the writer waits for a record before it looks at its other
/// work.
const TICK: Duration = Duration::from_secs(1);

/// How often the writer prunes without being asked.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// How many rows written before the writer prunes again.
const PRUNE_AFTER_ROWS: usize = 1000;

/// The most rows one pruning statement deletes, so the API's queries
/// needn't wait long for it.
const PRUNE_BATCH: i64 = 10_000;

/// The milliseconds in a day.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// Writes the records from `receiver` into the ledger over `connection`
/// until the sender is gone and every record is written.
pub(super) fn run(mut connection: Connection, receiver: &Receiver<UsageEvent>, shared: &Shared) {
    let secret = match client_key_secret(&connection) {
        Ok(secret) => secret,
        Err(error) => {
            tracing::warn!("usage ledger: read the client-key secret: {error}");
            Vec::new()
        }
    };
    prune_logged(&connection);
    let mut last_prune = Instant::now();
    let mut since_prune = 0usize;
    let mut batch = Vec::with_capacity(BATCH);
    loop {
        let disconnected = match receiver.recv_timeout(TICK) {
            Ok(event) => {
                batch.push(event);
                while batch.len() < BATCH {
                    match receiver.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        if !batch.is_empty() {
            match insert(&mut connection, &batch, &secret) {
                Ok(()) => since_prune += batch.len(),
                Err(error) => {
                    let lost = u64::try_from(batch.len()).unwrap_or(u64::MAX);
                    shared.write_failures.fetch_add(lost, Ordering::Relaxed);
                    tracing::warn!("usage ledger: write {lost} records: {error}");
                }
            }
            batch.clear();
        }
        if disconnected {
            break;
        }
        if shared.prune.swap(false, Ordering::Relaxed)
            || since_prune >= PRUNE_AFTER_ROWS
            || last_prune.elapsed() >= PRUNE_EVERY
        {
            prune_logged(&connection);
            last_prune = Instant::now();
            since_prune = 0;
        }
    }
}

/// Writes `events` as rows in one transaction.
pub(super) fn insert(
    connection: &mut Connection,
    events: &[UsageEvent],
    secret: &[u8],
) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    {
        let mut statement = transaction.prepare(
            "INSERT INTO requests (
                ts, request_id, endpoint, provider, model, alias,
                credential_id, auth_index, credential_label, auth_type,
                client_key_id, client_key_masked,
                stream, failed, status, latency_ms, ttft_ms,
                input_tokens, uncached_input_tokens, cache_read_tokens, cache_write_tokens,
                output_tokens, reasoning_tokens, unclassified_tokens, total_tokens
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
            )",
        )?;
        for event in events {
            let credential = event
                .credential
                .as_ref()
                .filter(|credential| !credential.id.is_empty());
            let (key_id, key_masked) = if event.client_key.is_empty() {
                (None, None)
            } else {
                let key = event.client_key.expose();
                (Some(client_key_id(secret, key)), Some(mask_client_key(key)))
            };
            let tokens = &event.tokens;
            statement.execute(params![
                event.requested_at.timestamp_millis(),
                event.request_id,
                event.endpoint,
                event.provider,
                event.model,
                event.alias,
                credential.map(|credential| credential.id.as_str()),
                credential.map(|credential| credential.auth_index.as_str()),
                credential.map(|credential| credential.label.as_str()),
                credential.map(|credential| credential.auth_type.as_str()),
                key_id,
                key_masked,
                event.stream,
                event.failed,
                event.status,
                millis(event.latency),
                event.ttft.map(millis),
                tokens.input.total_tokens,
                tokens.input.uncached_tokens,
                tokens.input.cache_read_tokens,
                tokens.input.cache_write_tokens,
                tokens.output.total_tokens,
                tokens.output.reasoning_tokens,
                tokens.unclassified_tokens,
                event.total_tokens,
            ])?;
        }
    }
    transaction.commit()
}

/// `duration` in whole milliseconds.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// A client key's ID in the ledger: `ck_` and the first 64 bits of the
/// key's HMAC-SHA256 under the ledger's secret, in hex.
pub(crate) fn client_key_id(secret: &[u8], key: &str) -> String {
    let mac = hmac_sha256(secret, key.as_bytes());
    let mut id = String::from("ck_");
    for byte in mac.iter().take(8) {
        let _ = write!(id, "{byte:02x}");
    }
    id
}

/// A client key as the ledger shows it: of a key of 32 characters or more,
/// its first three and last four; of 16 or more, its last four; of 8 or
/// more, its last two; of a shorter one, nothing. At most a quarter of a
/// key is shown, and never more than seven characters. `open-ferry`'s
/// agent commands show client keys so too, so a key reads the same there
/// as in the dashboard's usage.
pub fn mask_client_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let count = chars.len();
    let tail = |n: usize| -> String { chars.iter().skip(count.saturating_sub(n)).collect() };
    if count >= 32 {
        let head: String = chars.iter().take(3).collect();
        format!("{head}...{}", tail(4))
    } else if count >= 16 {
        format!("...{}", tail(4))
    } else if count >= 8 {
        format!("...{}", tail(2))
    } else {
        "...".to_owned()
    }
}

/// [`prune`], with a failure logged.
fn prune_logged(connection: &Connection) {
    if let Err(error) = prune(connection, Utc::now().timestamp_millis()) {
        tracing::warn!("usage ledger: prune: {error}");
    }
}

/// Deletes the rows older than the retention at `now_ms`, then the oldest
/// beyond the row cap, and gives the freed pages back. Returns how many
/// rows were deleted.
pub(super) fn prune(connection: &Connection, now_ms: i64) -> rusqlite::Result<u64> {
    let settings = read_settings(connection)?;
    let cutoff = now_ms.saturating_sub(settings.retention_days.saturating_mul(DAY_MS));
    let mut deleted = 0u64;
    loop {
        let count = connection.execute(
            "DELETE FROM requests WHERE id IN \
             (SELECT id FROM requests WHERE ts < ?1 LIMIT ?2)",
            params![cutoff, PRUNE_BATCH],
        )?;
        deleted += u64::try_from(count).unwrap_or(0);
        if i64::try_from(count).unwrap_or(i64::MAX) < PRUNE_BATCH {
            break;
        }
    }
    loop {
        let rows: i64 =
            connection.query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))?;
        let excess = rows.saturating_sub(settings.max_rows);
        if excess <= 0 {
            break;
        }
        let count = connection.execute(
            "DELETE FROM requests WHERE id IN \
             (SELECT id FROM requests ORDER BY ts, id LIMIT ?1)",
            params![excess.min(PRUNE_BATCH)],
        )?;
        deleted += u64::try_from(count).unwrap_or(0);
        if count == 0 {
            break;
        }
    }
    if deleted > 0 {
        connection.execute_batch("PRAGMA incremental_vacuum")?;
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Not upstream's: a key shows at most seven of its characters, and
    /// fewer the shorter it is.
    #[test]
    fn keys_are_masked_by_length() {
        for (key, masked) in [
            ("sk-0123456789abcdefghijklmnopqrstuv9f3k", "sk-...9f3k"),
            ("0123456789abcdef", "...cdef"),
            ("01234567", "...67"),
            ("0123456", "..."),
            ("", "..."),
            ("ключ-ключ-ключ-ключ", "...ключ"),
        ] {
            assert_eq!(mask_client_key(key), masked, "{key}");
        }
    }

    /// Not upstream's: a key's ID is the same for the same key and secret,
    /// and differs for another of either.
    #[test]
    fn key_ids_are_keyed() {
        let id = client_key_id(b"secret", "sk-test");
        assert_eq!(id.len(), 3 + 16);
        assert!(id.starts_with("ck_"));
        assert_eq!(id, client_key_id(b"secret", "sk-test"));
        assert_ne!(id, client_key_id(b"secret", "sk-test2"));
        assert_ne!(id, client_key_id(b"other", "sk-test"));
    }
}
