//! The ledger's file: opening it, its schema and its migrations, and its
//! settings.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

/// The schema this binary writes.
pub(crate) const SCHEMA_VERSION: i64 = 1;

/// How long a connection waits for the other's write before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Rows older than this many days are deleted, unless set otherwise.
pub(crate) const DEFAULT_RETENTION_DAYS: i64 = 90;

/// The oldest rows beyond this many are deleted, unless set otherwise.
pub(crate) const DEFAULT_MAX_ROWS: i64 = 1_000_000;

/// The currency shown beside costs, unless set otherwise.
const DEFAULT_CURRENCY: &str = "USD";

/// Schema version 1.
const V1: &str = "
CREATE TABLE settings (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
) WITHOUT ROWID;

CREATE TABLE prices (
    model TEXT PRIMARY KEY NOT NULL,
    input REAL NOT NULL,
    cache_read REAL,
    cache_write REAL,
    output REAL NOT NULL,
    updated INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE requests (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    request_id TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    alias TEXT NOT NULL,
    credential_id TEXT,
    auth_index TEXT,
    credential_label TEXT,
    auth_type TEXT,
    client_key_id TEXT,
    client_key_masked TEXT,
    stream INTEGER NOT NULL,
    failed INTEGER NOT NULL,
    status INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    ttft_ms INTEGER,
    input_tokens INTEGER NOT NULL,
    uncached_input_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    reasoning_tokens INTEGER NOT NULL,
    unclassified_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL
);

CREATE INDEX requests_ts ON requests (ts);
CREATE INDEX requests_model_ts ON requests (model, ts);
CREATE INDEX requests_provider_ts ON requests (provider, ts);
CREATE INDEX requests_credential_ts ON requests (credential_id, ts);
CREATE INDEX requests_client_key_ts ON requests (client_key_id, ts);
CREATE INDEX requests_request_id ON requests (request_id);
";

/// Opens the ledger at `path`, making it and its directory if need be, and
/// brings its schema up to date; the connection is the writer's. The error
/// says what failed, with the path.
pub(crate) fn open(path: &Path) -> Result<Connection, String> {
    let failed =
        |what: &str, error: &dyn std::fmt::Display| format!("{what} {}: {error}", path.display());
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).map_err(|error| failed("make the directory of", &error))?;
    }
    create_private(path).map_err(|error| failed("make", &error))?;
    let mut connection = Connection::open(path).map_err(|error| failed("open", &error))?;
    // Only a new file takes this; it must come before any table.
    connection
        .pragma_update(None, "auto_vacuum", "INCREMENTAL")
        .map_err(|error| failed("set up", &error))?;
    configure(&connection).map_err(|error| failed("set up", &error))?;
    migrate(&mut connection).map_err(|error| failed("update", &error))?;
    Ok(connection)
}

/// A second connection to the ledger at `path`, which [`open`] made.
pub(crate) fn connect(path: &Path) -> Result<Connection, String> {
    let connection =
        Connection::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    configure(&connection).map_err(|error| format!("set up {}: {error}", path.display()))?;
    Ok(connection)
}

/// Makes the file at `path` readable by its owner only, if it doesn't
/// exist yet; SQLite gives its journal files the same mode.
#[cfg(unix)]
fn create_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map(drop)
}

/// Nothing to do: the file is made as the directory allows.
#[cfg(not(unix))]
fn create_private(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The settings every connection needs: the write-ahead log, so reads
/// don't wait on the writer, and a wait for the other connection's write.
fn configure(connection: &Connection) -> rusqlite::Result<()> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!("usage ledger: journal mode is {mode}, not WAL");
    }
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Why a migration failed.
#[derive(Debug)]
enum MigrateError {
    Sqlite(rusqlite::Error),
    Newer(i64),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => error.fmt(f),
            Self::Newer(version) => write!(
                f,
                "its schema is version {version}, and this open-ferry knows only up to \
                 {SCHEMA_VERSION}; a newer open-ferry made it"
            ),
        }
    }
}

impl From<rusqlite::Error> for MigrateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Brings the schema up to [`SCHEMA_VERSION`] and fills in the settings
/// that are missing, the client-key secret among them.
fn migrate(connection: &mut Connection) -> Result<(), MigrateError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction
        .execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
    let version: Option<i64> =
        transaction.query_row("SELECT MAX(version) FROM schema_version", [], |row| {
            row.get(0)
        })?;
    match version.unwrap_or(0) {
        0 => {
            transaction.execute_batch(V1)?;
            transaction.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        }
        SCHEMA_VERSION => {}
        newer => return Err(MigrateError::Newer(newer)),
    }
    let defaults = [
        (RETENTION_DAYS, DEFAULT_RETENTION_DAYS.to_string()),
        (MAX_ROWS, DEFAULT_MAX_ROWS.to_string()),
        (CURRENCY, DEFAULT_CURRENCY.to_owned()),
        (CLIENT_KEY_SECRET, new_secret()),
    ];
    for (key, value) in defaults {
        transaction.execute(
            "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// 32 random bytes in hex.
fn new_secret() -> String {
    let bytes: [u8; 32] = rand::random();
    let mut text = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

const RETENTION_DAYS: &str = "retention_days";
const MAX_ROWS: &str = "max_rows";
const CURRENCY: &str = "currency";
const CLIENT_KEY_SECRET: &str = "client_key_secret";

/// The ledger's settings the user may change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Rows older than this many days are deleted.
    pub(crate) retention_days: i64,
    /// The oldest rows beyond this many are deleted.
    pub(crate) max_rows: i64,
    /// The currency shown beside costs.
    pub(crate) currency: String,
}

/// The settings in the ledger; a missing or unreadable one is its default.
pub(crate) fn read_settings(connection: &Connection) -> rusqlite::Result<Settings> {
    let number = |key: &str, default: i64| -> rusqlite::Result<i64> {
        Ok(setting(connection, key)?
            .and_then(|value| value.parse().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default))
    };
    Ok(Settings {
        retention_days: number(RETENTION_DAYS, DEFAULT_RETENTION_DAYS)?,
        max_rows: number(MAX_ROWS, DEFAULT_MAX_ROWS)?,
        currency: setting(connection, CURRENCY)?.unwrap_or_else(|| DEFAULT_CURRENCY.to_owned()),
    })
}

/// Writes `settings` to the ledger.
pub(crate) fn write_settings(
    connection: &mut Connection,
    settings: &Settings,
) -> rusqlite::Result<()> {
    let transaction = connection.transaction()?;
    for (key, value) in [
        (RETENTION_DAYS, settings.retention_days.to_string()),
        (MAX_ROWS, settings.max_rows.to_string()),
        (CURRENCY, settings.currency.clone()),
    ] {
        transaction.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }
    transaction.commit()
}

/// The secret that keys the client keys' hashes.
pub(crate) fn client_key_secret(connection: &Connection) -> rusqlite::Result<Vec<u8>> {
    Ok(setting(connection, CLIENT_KEY_SECRET)?
        .unwrap_or_default()
        .into_bytes())
}

/// The setting `key`, if it is set.
fn setting(connection: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
}
