use crate::i18n::tr;
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior};
use std::path::Path;

pub const SCHEMA_VERSION: u32 = 1;
pub const FILE_NAME: &str = "orbment.db";

pub fn open(data_dir: &Path) -> Result<Connection> {
    std::fs::create_dir_all(data_dir).with_context(|| tr("db.open_failed"))?;
    let mut connection =
        Connection::open(data_dir.join(FILE_NAME)).with_context(|| tr("db.open_failed"))?;
    initialize(&mut connection)?;
    Ok(connection)
}

pub fn initialize(connection: &mut Connection) -> Result<()> {
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA journal_size_limit = 67108864;",
        )
        .with_context(|| tr("db.initialize_failed"))?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version <= SCHEMA_VERSION,
        "{}: {version}",
        tr("db.newer_schema")
    );
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    // Recheck under the write lock: another process may have migrated since the read.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version <= SCHEMA_VERSION,
        "{}: {version}",
        tr("db.newer_schema")
    );
    if version < 1 {
        transaction
            .execute_batch(SCHEMA_V1)
            .with_context(|| tr("db.migration_failed"))?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    transaction
        .commit()
        .with_context(|| tr("db.migration_failed"))?;
    Ok(())
}

/// Opportunistic idle checkpoint; active readers and writers are never waited on.
pub fn checkpoint(connection: &Connection) -> Result<()> {
    connection
        .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
        .with_context(|| tr("db.checkpoint_failed"))
}

/// Call after background writers have stopped at orderly shutdown.
pub fn checkpoint_truncate(connection: &Connection) -> Result<()> {
    let busy: i32 =
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
    ensure!(busy == 0, "{}", tr("db.checkpoint_busy"));
    Ok(())
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS clipboard_groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    icon TEXT NOT NULL,
    is_hidden INTEGER NOT NULL DEFAULT 0,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clipboard_apps (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    bundle_or_exe TEXT UNIQUE COLLATE NOCASE,
    icon_file TEXT,
    platform TEXT NOT NULL DEFAULT 'windows',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS clipboard_items (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    sub_kind TEXT,
    group_id TEXT REFERENCES clipboard_groups(id) ON DELETE SET NULL,
    source_app_id TEXT REFERENCES clipboard_apps(id) ON DELETE SET NULL,
    content TEXT NOT NULL,
    raw_payload BLOB,
    content_hash TEXT NOT NULL,
    search_text TEXT,
    summary TEXT,
    file_types TEXT,
    size INTEGER,
    width INTEGER,
    height INTEGER,
    use_count INTEGER NOT NULL DEFAULT 1,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    is_pinned INTEGER NOT NULL DEFAULT 0,
    is_sensitive INTEGER NOT NULL DEFAULT 0,
    platform TEXT NOT NULL DEFAULT 'windows',
    note TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    CHECK (kind <> 'files' OR trim(content) <> '')
);
CREATE INDEX IF NOT EXISTS idx_clipboard_items_updated_at ON clipboard_items(is_pinned DESC, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_clipboard_items_kind_group ON clipboard_items(kind, group_id, is_favorite);
CREATE INDEX IF NOT EXISTS idx_clipboard_items_hash ON clipboard_items(content_hash);
CREATE VIRTUAL TABLE IF NOT EXISTS clipboard_items_fts USING fts5(
    search_text,
    note,
    content='clipboard_items',
    content_rowid='seq',
    tokenize='trigram'
);
CREATE TRIGGER IF NOT EXISTS trg_clipboard_items_ai AFTER INSERT ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts(rowid, search_text, note) VALUES (new.seq, new.search_text, new.note);
END;
CREATE TRIGGER IF NOT EXISTS trg_clipboard_items_ad AFTER DELETE ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts(clipboard_items_fts, rowid, search_text, note) VALUES ('delete', old.seq, old.search_text, old.note);
END;
CREATE TRIGGER IF NOT EXISTS trg_clipboard_items_au AFTER UPDATE OF search_text, note ON clipboard_items BEGIN
    INSERT INTO clipboard_items_fts(clipboard_items_fts, rowid, search_text, note) VALUES ('delete', old.seq, old.search_text, old.note);
    INSERT INTO clipboard_items_fts(rowid, search_text, note) VALUES (new.seq, new.search_text, new.note);
END;
CREATE TABLE IF NOT EXISTS file_type_icons (
    cache_key TEXT NOT NULL,
    platform TEXT NOT NULL DEFAULT 'windows',
    icon_file TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (cache_key, platform)
);
CREATE TABLE IF NOT EXISTS traffic_daily_history (
    day_date TEXT PRIMARY KEY,
    upload_bytes INTEGER NOT NULL DEFAULT 0,
    download_bytes INTEGER NOT NULL DEFAULT 0,
    adapter_mode TEXT NOT NULL DEFAULT 'auto',
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pin_records (
    id TEXT PRIMARY KEY,
    pin_kind TEXT NOT NULL,
    group_name TEXT NOT NULL DEFAULT 'default',
    title TEXT,
    image_rel_path TEXT,
    text_content TEXT,
    annotations_json TEXT,
    monitor_name TEXT,
    monitor_dpi INTEGER NOT NULL DEFAULT 96,
    pos_x INTEGER NOT NULL,
    pos_y INTEGER NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    scale_pct REAL NOT NULL DEFAULT 100.0,
    opacity_pct REAL NOT NULL DEFAULT 100.0,
    is_topmost INTEGER NOT NULL DEFAULT 1,
    is_locked INTEGER NOT NULL DEFAULT 0,
    is_penetrate INTEGER NOT NULL DEFAULT 0,
    is_thumbnail INTEGER NOT NULL DEFAULT 0,
    is_active_pin INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    closed_at TEXT
);
CREATE TABLE IF NOT EXISTS file_run_history (
    full_path TEXT PRIMARY KEY COLLATE NOCASE,
    run_count INTEGER NOT NULL DEFAULT 1,
    last_run_date TEXT NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(connection: &Connection, text: &str) -> rusqlite::Result<i64> {
        connection.query_row(
            "SELECT count(*) FROM clipboard_items_fts WHERE clipboard_items_fts MATCH ?1",
            [text],
            |row| row.get(0),
        )
    }

    #[test]
    fn stable_fts_mapping_triggers_and_foreign_keys() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        initialize(&mut connection)?;
        connection.execute_batch(
            "INSERT INTO clipboard_groups VALUES ('group', 'group', '', 0, 0, 'now', 'now');
             INSERT INTO clipboard_items(seq,id,kind,group_id,content,content_hash,search_text,created_at,updated_at)
             VALUES (42,'item','text','group','original','key','original','now','now');",
        )?;
        assert_eq!(matches(&connection, "original")?, 1);
        connection.execute_batch(
            "UPDATE clipboard_items SET search_text='replacement', note='annotation'; VACUUM;",
        )?;
        assert_eq!(matches(&connection, "original")?, 0);
        assert_eq!(matches(&connection, "replacement")?, 1);
        assert_eq!(matches(&connection, "annotation")?, 1);
        let seq: i64 =
            connection.query_row("SELECT seq FROM clipboard_items", [], |row| row.get(0))?;
        assert_eq!(seq, 42);
        connection.execute_batch(
            "DELETE FROM clipboard_groups; UPDATE clipboard_items SET use_count=2;",
        )?;
        let group: Option<String> =
            connection.query_row("SELECT group_id FROM clipboard_items", [], |row| row.get(0))?;
        assert!(group.is_none());
        assert_eq!(matches(&connection, "replacement")?, 1);
        connection.execute_batch("DELETE FROM clipboard_items")?;
        assert_eq!(matches(&connection, "replacement")?, 0);
        assert!(connection.execute_batch("INSERT INTO clipboard_items(id,kind,content,content_hash,created_at,updated_at) VALUES ('bad','files','  ','key','now','now')").is_err());
        Ok(())
    }

    #[test]
    fn opening_current_schema_does_not_wait_for_another_writer() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "orbment-db-reader-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let result = (|| -> Result<()> {
            let mut writer = open(&directory)?;
            let transaction = writer.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO file_run_history VALUES ('pending', 1, 'now')",
                [],
            )?;
            let reader = open(&directory)?;
            let count: i64 =
                reader.query_row("SELECT count(*) FROM file_run_history", [], |row| row.get(0))?;
            assert_eq!(count, 0);
            Ok(())
        })();
        std::fs::remove_dir_all(directory)?;
        result
    }

    #[test]
    fn migration_is_atomic_idempotent_and_rejects_future_versions() -> Result<()> {
        let mut connection = Connection::open_in_memory()?;
        connection.execute_batch("CREATE TABLE clipboard_items (wrong_column TEXT)")?;
        assert!(initialize(&mut connection).is_err());
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 0);
        let groups: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='clipboard_groups'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(groups, 0);
        connection.execute_batch("DROP TABLE clipboard_items")?;
        initialize(&mut connection)?;
        initialize(&mut connection)?;
        let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, SCHEMA_VERSION);
        connection.pragma_update(None, "user_version", SCHEMA_VERSION + 1)?;
        assert!(initialize(&mut connection).is_err());
        Ok(())
    }
}
