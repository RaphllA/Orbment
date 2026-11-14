use crate::content::{display_text, is_sensitive, truncate_search_text};
use crate::types::*;
use anyhow::{Context, Result, ensure};
use chrono::{Duration, Months, SecondsFormat, Utc};
use orbment_core::clipboard_content::{content_hash, image_relative_path};
use orbment_core::{
    config::{ClipboardConfig, ClipboardDeletePolicy},
    db,
    i18n::tr,
};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, params, params_from_iter, types::Value,
};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};
use uuid::Uuid;

const COLUMNS: &str = "i.seq,i.id,i.kind,i.sub_kind,i.group_id,i.source_app_id,i.content,i.raw_payload,i.search_text,i.summary,i.size,i.width,i.height,i.use_count,i.is_favorite,i.is_pinned,i.is_sensitive,i.note,i.created_at,i.updated_at,a.name";
/// Item rows joined with their source application, which supplies the display name.
const ITEMS: &str = "clipboard_items i LEFT JOIN clipboard_apps a ON a.id=i.source_app_id";
type ChangeCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct ClipboardStore {
    write_connection: Arc<Mutex<Connection>>,
    read_connection: Arc<Mutex<Connection>>,
    data_dir: PathBuf,
    config: Arc<ClipboardConfig>,
    on_change: Arc<Mutex<Option<ChangeCallback>>>,
}

impl ClipboardStore {
    pub fn open(data_dir: &Path, config: &ClipboardConfig) -> Result<Self> {
        config.validate()?;
        let write_connection = db::open(data_dir)?;
        let read_connection = Connection::open_with_flags(
            data_dir.join(db::FILE_NAME),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| tr("db.open_failed"))?;
        Ok(Self {
            write_connection: Arc::new(Mutex::new(write_connection)),
            read_connection: Arc::new(Mutex::new(read_connection)),
            data_dir: data_dir.to_owned(),
            config: Arc::new(config.clone()),
            on_change: Arc::new(Mutex::new(None)),
        })
    }

    fn write_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.write_connection
            .lock()
            .map_err(|_| anyhow::anyhow!(tr("clipboard.error.store_poisoned")))
    }

    fn read_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.read_connection
            .lock()
            .map_err(|_| anyhow::anyhow!(tr("clipboard.error.store_poisoned")))
    }

    pub fn set_on_change(&self, callback: ChangeCallback) {
        if let Ok(mut slot) = self.on_change.lock() {
            *slot = Some(callback);
        }
    }

    fn changed(&self) {
        let callback = self.on_change.lock().ok().and_then(|slot| slot.clone());
        if let Some(callback) = callback {
            callback();
        }
    }

    pub fn get(&self, seq: i64) -> Result<Option<ClipboardItem>> {
        Ok(self
            .read_connection()?
            .query_row(
                &format!("SELECT {COLUMNS} FROM {ITEMS} WHERE i.seq=?1"),
                [seq],
                item_from_row,
            )
            .optional()?)
    }

    pub fn latest(&self) -> Result<Option<ClipboardItem>> {
        Ok(self
            .read_connection()?
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM {ITEMS} ORDER BY i.created_at DESC,i.seq DESC LIMIT 1"
                ),
                [],
                item_from_row,
            )
            .optional()?)
    }

    pub fn checkpoint(&self) -> Result<()> {
        let connection = self.write_connection()?;
        db::checkpoint(&connection)
    }

    pub fn query(&self, query: &ClipboardQuery) -> Result<Vec<ClipboardItem>> {
        let (filter, mut values) = query_filter(query);
        let sort = match self.config.content.sort.as_str() {
            "createdAtDesc" => "i.created_at DESC",
            "useCountDesc" => "i.use_count DESC",
            _ => "i.updated_at DESC",
        };
        // List pages never materialize potentially multi-megabyte rich payloads; get() serves previews.
        let columns = COLUMNS.replace("i.raw_payload", "NULL");
        let sql = format!(
            "SELECT {columns} FROM {ITEMS} WHERE {filter} ORDER BY i.is_pinned DESC,{sort},i.seq DESC LIMIT ? OFFSET ?"
        );
        values.push(Value::Integer(query.limit.clamp(1, 1000) as i64));
        values.push(Value::Integer(query.offset as i64));
        let connection = self.read_connection()?;
        let mut statement = connection.prepare(&sql)?;
        Ok(statement
            .query_map(params_from_iter(values), item_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn count(&self, query: &ClipboardQuery) -> Result<u64> {
        let (filter, values) = query_filter(query);
        Ok(self.read_connection()?.query_row(
            &format!("SELECT count(*) FROM clipboard_items i WHERE {filter}"),
            params_from_iter(values),
            |row| row.get(0),
        )?)
    }

    pub fn display_content(&self, item: &ClipboardItem) -> String {
        display_text(
            &item.content,
            item.is_sensitive,
            self.config.sensitive.redact_secrets,
        )
    }

    pub fn media_path(&self, item: &ClipboardItem) -> Result<Option<PathBuf>> {
        if item.kind != "image" {
            return Ok(None);
        }
        Ok(Some(self.resolve_media(&item.content)?))
    }

    fn resolve_media(&self, relative: &str) -> Result<PathBuf> {
        let normalized = relative.replace('\\', "/");
        let relative = Path::new(&normalized);
        ensure!(
            relative
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
                && relative.starts_with("media"),
            "{}",
            tr("clipboard.error.invalid_media")
        );
        let mut path = self.data_dir.clone();
        for part in relative.components() {
            if let Component::Normal(part) = part {
                path.push(part);
            }
        }
        let root = self.data_dir.join("media");
        // A drive-prefixed component must not replace the trusted root on Windows.
        ensure!(
            path.starts_with(&root),
            "{}",
            tr("clipboard.error.invalid_media")
        );
        if path.exists() {
            let root = root.canonicalize()?;
            ensure!(
                path.canonicalize()?.starts_with(root),
                "{}",
                tr("clipboard.error.invalid_media")
            );
        }
        Ok(path)
    }

    pub fn write_content(&self, seq: i64, options: WriteOptions) -> Result<WriteContent> {
        let item = self.get(seq)?.context(tr("clipboard.error.item_missing"))?;
        Ok(match item.kind.as_str() {
            "image" if options.plain_text => {
                WriteContent::Text(item.search_text.unwrap_or_default())
            }
            "image" => WriteContent::Image(self.resolve_media(&item.content)?),
            "files" => {
                let paths: Vec<PathBuf> = serde_json::from_str(&item.content)?;
                if options.files_as_paths || options.plain_text {
                    WriteContent::Text(
                        paths
                            .iter()
                            .map(|p| p.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("\r\n"),
                    )
                } else {
                    WriteContent::Files(paths)
                }
            }
            _ if !options.plain_text
                && item.sub_kind.as_deref() == Some("html")
                && item.raw_payload.is_some() =>
            {
                WriteContent::Html {
                    plain: item.content,
                    raw: item.raw_payload.unwrap_or_default(),
                }
            }
            _ if !options.plain_text
                && item.sub_kind.as_deref() == Some("rtf")
                && item.raw_payload.is_some() =>
            {
                WriteContent::Rtf {
                    plain: item.content,
                    raw: item.raw_payload.unwrap_or_default(),
                }
            }
            _ => WriteContent::Text(item.content),
        })
    }

    pub fn record_use(&self, seq: i64) -> Result<()> {
        let now = now();
        self.write_connection()?.execute("UPDATE clipboard_items SET use_count=use_count+1,updated_at=CASE WHEN ?2 THEN ?3 ELSE updated_at END,is_favorite=CASE WHEN ?4 THEN 1 ELSE is_favorite END WHERE seq=?1", params![seq,self.config.content.update_on_reuse,now,self.config.content.auto_favorite])?;
        self.changed();
        Ok(())
    }

    pub fn insert_text(&self, text: &str) -> Result<Option<i64>> {
        self.validate_text(text)?;
        self.save_capture(Captured {
            kind: "text".into(),
            sub_kind: Some(crate::content::sub_kind(text).into()),
            content: text.to_owned(),
            raw_payload: None,
            search_text: text.to_owned(),
            size: text.encode_utf16().count() as u64,
            dimensions: None,
            source: None,
        })
    }

    pub fn update_text(&self, seq: i64, text: &str) -> Result<()> {
        self.validate_text(text)?;
        let sensitive = is_sensitive(text);
        ensure!(
            !sensitive || self.config.sensitive.collect_secrets,
            "{}",
            tr("clipboard.error.sensitive_disabled")
        );
        let hash = content_hash("text", text);
        let mut connection = self.write_connection()?;
        let transaction = connection.transaction()?;
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM clipboard_items WHERE content_hash=?1 AND seq<>?2)",
            params![hash, seq],
            |row| row.get(0),
        )?;
        ensure!(!duplicate, "{}", tr("clipboard.error.duplicate_item"));
        let affected = transaction.execute("UPDATE clipboard_items SET content=?2,sub_kind=?3,raw_payload=NULL,content_hash=?4,search_text=?5,summary=?6,size=?7,is_sensitive=?8,updated_at=?9,is_favorite=CASE WHEN ?10 THEN 1 ELSE is_favorite END WHERE seq=?1 AND kind='text'",params![seq,text,crate::content::sub_kind(text),hash,truncate_search_text(text),text.chars().take(240).collect::<String>(),text.encode_utf16().count() as i64,sensitive,now(),self.config.content.auto_favorite])?;
        ensure!(affected != 0, "{}", tr("clipboard.error.item_missing"));
        transaction.commit()?;
        drop(connection);
        self.changed();
        Ok(())
    }

    fn validate_text(&self, text: &str) -> Result<()> {
        ensure!(
            !text.is_empty() && !text.contains('\0'),
            "{}",
            tr("clipboard.error.invalid_content")
        );
        let maximum = self.config.capture.max_text_mb;
        ensure!(
            maximum == 0
                || text.encode_utf16().count() as u64 * 2 <= u64::from(maximum) * 1024 * 1024,
            "{}",
            tr("clipboard.error.too_large")
        );
        Ok(())
    }

    pub fn set_favorite(&self, seq: i64, value: bool) -> Result<()> {
        self.set_flag(seq, "is_favorite", value)
    }
    pub fn set_pinned(&self, seq: i64, value: bool) -> Result<()> {
        self.set_flag(seq, "is_pinned", value)
    }
    fn set_flag(&self, seq: i64, column: &str, value: bool) -> Result<()> {
        let affected = self.write_connection()?.execute(
            &format!("UPDATE clipboard_items SET {column}=?2 WHERE seq=?1"),
            params![seq, value],
        )?;
        ensure!(affected != 0, "{}", tr("clipboard.error.item_missing"));
        self.changed();
        Ok(())
    }

    pub fn set_note(&self, seq: i64, note: &str) -> Result<()> {
        ensure!(
            note.len() <= 64 * 1024,
            "{}",
            tr("clipboard.error.too_large")
        );
        let affected = self.write_connection()?.execute("UPDATE clipboard_items SET note=?2,is_favorite=CASE WHEN ?3 THEN 1 ELSE is_favorite END WHERE seq=?1",params![seq,note,self.config.content.auto_favorite])?;
        ensure!(affected != 0, "{}", tr("clipboard.error.item_missing"));
        self.changed();
        Ok(())
    }

    pub fn set_group(&self, seq: i64, group_id: Option<&str>) -> Result<()> {
        let affected = self.write_connection()?.execute(
            "UPDATE clipboard_items SET group_id=?2 WHERE seq=?1",
            params![seq, group_id],
        )?;
        ensure!(affected != 0, "{}", tr("clipboard.error.item_missing"));
        self.changed();
        Ok(())
    }

    pub fn groups(&self) -> Result<Vec<ClipboardGroup>> {
        let connection = self.read_connection()?;
        let mut statement = connection.prepare("SELECT id,name,icon,is_hidden,sort_order FROM clipboard_groups ORDER BY sort_order,created_at,id")?;
        Ok(statement
            .query_map([], |row| {
                Ok(ClipboardGroup {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    icon: row.get(2)?,
                    is_hidden: row.get(3)?,
                    sort_order: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn save_group(&self, group: &ClipboardGroup) -> Result<()> {
        ensure!(
            !group.id.is_empty() && !group.name.trim().is_empty(),
            "{}",
            tr("clipboard.error.invalid_group")
        );
        self.write_connection()?.execute("INSERT INTO clipboard_groups(id,name,icon,is_hidden,sort_order,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,icon=excluded.icon,is_hidden=excluded.is_hidden,sort_order=excluded.sort_order,updated_at=excluded.updated_at", params![group.id,group.name,group.icon,group.is_hidden,group.sort_order,now()])?;
        self.changed();
        Ok(())
    }

    pub fn create_group(&self, name: &str, icon: &str) -> Result<ClipboardGroup> {
        let group = ClipboardGroup {
            id: Uuid::new_v4().to_string(),
            name: name.to_owned(),
            icon: icon.to_owned(),
            is_hidden: false,
            sort_order: 0,
        };
        self.save_group(&group)?;
        Ok(group)
    }

    pub fn delete_group(&self, id: &str) -> Result<()> {
        self.write_connection()?
            .execute("DELETE FROM clipboard_groups WHERE id=?1", [id])?;
        self.changed();
        Ok(())
    }

    /// All selected rows are checked before deleting any, so a protected item aborts the whole request.
    pub fn delete(&self, seqs: &[i64], options: DeleteOptions) -> Result<usize> {
        let mut connection = self.write_connection()?;
        let transaction = connection.transaction()?;
        let mut images = HashSet::new();
        for seq in seqs {
            let flags = transaction
                .query_row(
                    "SELECT is_favorite,is_pinned,CASE WHEN kind='image' THEN content END FROM clipboard_items WHERE seq=?1",
                    [seq],
                    |row| {
                        Ok((
                            row.get::<_, bool>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((favorite, pinned, image)) = flags else {
                continue;
            };
            let policy =
                self.config
                    .content
                    .delete_policy(favorite, pinned, options.in_favorites_group);
            ensure!(
                policy != ClipboardDeletePolicy::Protected,
                "{}",
                tr("clipboard.error.protected_item")
            );
            ensure!(
                policy != ClipboardDeletePolicy::ConfirmationRequired || options.confirmed,
                "{}",
                tr("clipboard.error.confirm_delete")
            );
            if let Some(image) = image {
                images.insert(image);
            }
        }
        let mut count = 0;
        for seq in seqs {
            count += transaction.execute("DELETE FROM clipboard_items WHERE seq=?1", [seq])?;
        }
        let mut media_paths = Vec::new();
        if !images.is_empty() {
            let references = media_references(&transaction)?;
            for relative in images {
                if !references.contains(&media_reference_key(&relative)) {
                    media_paths.push(self.resolve_media(&relative)?);
                }
            }
        }
        transaction.commit()?;
        for path in media_paths {
            match path.symlink_metadata() {
                Ok(metadata) if metadata.is_file() => std::fs::remove_file(path)?,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        drop(connection);
        if count > 0 {
            self.changed();
        }
        Ok(count)
    }

    pub fn cleanup(&self) -> Result<CleanupReport> {
        let mut connection = self.write_connection()?;
        let transaction = connection.transaction()?;
        let history = &self.config.history;
        let current = Utc::now();
        let cutoff = match history.unit.as_str() {
            "hour" | "hours" => {
                current.checked_sub_signed(Duration::hours(history.retention_value as i64))
            }
            "day" | "days" => {
                current.checked_sub_signed(Duration::days(history.retention_value as i64))
            }
            "week" | "weeks" => {
                current.checked_sub_signed(Duration::weeks(history.retention_value as i64))
            }
            "month" | "months" => current.checked_sub_months(Months::new(history.retention_value)),
            _ => None,
        };
        // Unattended cleanup cannot satisfy interactive favorite/pin confirmation policies.
        let policy = &self.config.content;
        let allow_favorite = policy.delete_favorite_items
            && !policy.delete_favorite_items_confirm
            && !policy.delete_favorite_items_only_in_fav_group;
        let allow_pinned = policy.delete_pinned_items && !policy.delete_pinned_items_confirm;
        let mut report = CleanupReport::default();
        if history.retention_value > 0
            && let Some(cutoff) = cutoff
        {
            report.deleted_items += transaction.execute("DELETE FROM clipboard_items WHERE (is_favorite=0 OR ?2) AND (is_pinned=0 OR ?3) AND updated_at<?1", params![cutoff.to_rfc3339_opts(SecondsFormat::Millis,true),allow_favorite,allow_pinned])?;
        }
        if history.max_count > 0 {
            let total: i64 =
                transaction
                    .query_row("SELECT count(*) FROM clipboard_items", [], |row| row.get(0))?;
            let excess = (total - history.max_count as i64).max(0);
            report.deleted_items += transaction.execute("DELETE FROM clipboard_items WHERE seq IN(SELECT seq FROM clipboard_items WHERE (is_favorite=0 OR ?2) AND (is_pinned=0 OR ?3) ORDER BY updated_at ASC,seq ASC LIMIT ?1)", params![excess,allow_favorite,allow_pinned])?;
        }
        transaction.commit()?;
        report.deleted_media = self.clean_media_locked(&connection)?;
        db::checkpoint(&connection)?;
        drop(connection);
        if report.deleted_items > 0 {
            self.changed();
        }
        Ok(report)
    }

    fn clean_media_locked(&self, connection: &Connection) -> Result<usize> {
        let references = media_references(connection)?;
        let mut deleted = 0;
        for folder in ["clipboard_images", "clipboard_icons"] {
            let directory = self.data_dir.join("media").join(folder);
            if !directory.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let relative = format!("media/{folder}/{}", entry.file_name().to_string_lossy());
                if !references.contains(&media_reference_key(&relative)) {
                    std::fs::remove_file(entry.path())?;
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    }

    pub(crate) fn save_capture(&self, captured: Captured) -> Result<Option<i64>> {
        let mut connection = self.write_connection()?;
        let seq = self.save_capture_locked(&mut connection, captured)?;
        drop(connection);
        if seq.is_some() {
            self.changed();
        }
        Ok(seq)
    }

    fn save_capture_locked(
        &self,
        connection: &mut Connection,
        captured: Captured,
    ) -> Result<Option<i64>> {
        let sensitive = is_sensitive(&captured.content)
            || is_sensitive(&captured.search_text)
            || captured
                .raw_payload
                .as_deref()
                .and_then(|raw| std::str::from_utf8(raw).ok())
                .is_some_and(is_sensitive);
        if sensitive && !self.config.sensitive.collect_secrets {
            return Ok(None);
        }
        let hash = content_hash(&captured.kind, &captured.content);
        let transaction = connection.transaction()?;
        let existing: Option<i64> = transaction
            .query_row(
                "SELECT seq FROM clipboard_items WHERE content_hash=?1 ORDER BY seq LIMIT 1",
                [&hash],
                |row| row.get(0),
            )
            .optional()?;
        let timestamp = now();
        let source_id = if let Some(source) = captured.source {
            let id = Uuid::new_v4().to_string();
            transaction.execute("INSERT INTO clipboard_apps(id,name,bundle_or_exe,icon_file,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(bundle_or_exe) DO UPDATE SET name=excluded.name,icon_file=coalesce(clipboard_apps.icon_file,excluded.icon_file),updated_at=excluded.updated_at", params![id,source.name,source.executable,source.icon_file,timestamp])?;
            Some(transaction.query_row(
                "SELECT id FROM clipboard_apps WHERE bundle_or_exe=?1",
                [source.executable],
                |row| row.get::<_, String>(0),
            )?)
        } else {
            None
        };
        let seq = if let Some(seq) = existing {
            // Prefer the richest newly captured representation while retaining annotations and OCR.
            transaction.execute("UPDATE clipboard_items SET use_count=use_count+1,updated_at=CASE WHEN ?2 THEN ?3 ELSE updated_at END,source_app_id=coalesce(?4,source_app_id),raw_payload=coalesce(?5,raw_payload),sub_kind=CASE WHEN ?5 IS NOT NULL THEN ?6 ELSE sub_kind END,is_sensitive=max(is_sensitive,?7),is_favorite=CASE WHEN ?8 THEN 1 ELSE is_favorite END WHERE seq=?1", params![seq,self.config.content.update_on_reuse,timestamp,source_id,captured.raw_payload,captured.sub_kind,sensitive,self.config.content.auto_favorite])?;
            seq
        } else {
            let file_types = if captured.kind == "files" {
                let paths: Vec<PathBuf> = serde_json::from_str(&captured.content)?;
                let mut types: Vec<String> = paths
                    .iter()
                    .filter_map(|path| {
                        path.extension()
                            .map(|ext| ext.to_string_lossy().to_lowercase())
                    })
                    .collect();
                types.sort();
                types.dedup();
                Some(serde_json::to_string(&types)?)
            } else {
                None
            };
            transaction.execute("INSERT INTO clipboard_items(id,kind,sub_kind,source_app_id,content,raw_payload,content_hash,search_text,summary,file_types,size,width,height,is_sensitive,is_favorite,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)", params![Uuid::new_v4().to_string(),captured.kind,captured.sub_kind,source_id,captured.content,captured.raw_payload,hash,truncate_search_text(&captured.search_text),captured.search_text.chars().take(240).collect::<String>(),file_types,captured.size.min(i64::MAX as u64) as i64,captured.dimensions.map(|v|v.0),captured.dimensions.map(|v|v.1),sensitive,self.config.content.auto_favorite,timestamp])?;
            transaction.last_insert_rowid()
        };
        transaction.commit()?;
        Ok(Some(seq))
    }

    pub(crate) fn set_ocr_text(&self, seq: i64, expected_content: &str, text: &str) -> Result<()> {
        let sensitive = is_sensitive(text);
        let connection = self.write_connection()?;
        if sensitive && !self.config.sensitive.collect_secrets {
            connection.execute("DELETE FROM clipboard_items WHERE seq=?1 AND content=?2 AND is_favorite=0 AND is_pinned=0",params![seq,expected_content])?;
            connection.execute(
                "UPDATE clipboard_items SET is_sensitive=1 WHERE seq=?1 AND content=?2",
                params![seq, expected_content],
            )?;
            self.clean_media_locked(&connection)?;
        } else {
            connection.execute("UPDATE clipboard_items SET search_text=?3,summary=?4,is_sensitive=?5 WHERE seq=?1 AND kind='image' AND content=?2",params![seq,expected_content,truncate_search_text(text),text.chars().take(240).collect::<String>(),sensitive])?;
        }
        drop(connection);
        self.changed();
        Ok(())
    }

    pub(crate) fn save_png_capture(
        &self,
        bytes: &[u8],
        dimensions: (u32, u32),
        source: Option<SourceApp>,
    ) -> Result<Option<i64>> {
        let relative = image_relative_path(bytes);
        let path = self.data_dir.join(&relative);
        // The writer mutex serializes file creation, row commit and media reclamation.
        let mut connection = self.write_connection()?;
        std::fs::create_dir_all(path.parent().context(tr("clipboard.error.invalid_media"))?)?;
        if !path.exists() {
            write_media_atomic(&path, bytes)?;
        }
        let seq = self.save_capture_locked(
            &mut connection,
            Captured {
                kind: "image".into(),
                sub_kind: None,
                content: relative,
                raw_payload: None,
                search_text: String::new(),
                size: bytes.len() as u64,
                dimensions: Some(dimensions),
                source,
            },
        )?;
        drop(connection);
        if seq.is_some() {
            self.changed();
        }
        Ok(seq)
    }

    pub(crate) fn source_icon(&self, executable: &str) -> Result<Option<String>> {
        Ok(self
            .read_connection()?
            .query_row(
                "SELECT icon_file FROM clipboard_apps WHERE bundle_or_exe=?1",
                [executable],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    pub(crate) fn source_excluded(
        &self,
        executable: &str,
        excluded_ids: &[String],
    ) -> Result<bool> {
        if excluded_ids.is_empty() {
            return Ok(false);
        }
        let id: Option<String> = self
            .read_connection()?
            .query_row(
                "SELECT id FROM clipboard_apps WHERE bundle_or_exe=?1",
                [executable],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id.is_some_and(|id| {
            excluded_ids
                .iter()
                .any(|excluded| excluded.eq_ignore_ascii_case(&id))
        }))
    }

    pub(crate) fn save_source_icon(&self, source: &SourceApp, bytes: &[u8]) -> Result<String> {
        let connection = self.write_connection()?;
        let relative = format!("media/clipboard_icons/{}.png", Uuid::new_v4());
        let path = self.data_dir.join(&relative);
        std::fs::create_dir_all(path.parent().context(tr("clipboard.error.invalid_media"))?)?;
        std::fs::write(path, bytes)?;
        connection.execute("INSERT INTO clipboard_apps(id,name,bundle_or_exe,icon_file,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(bundle_or_exe) DO UPDATE SET icon_file=excluded.icon_file,updated_at=excluded.updated_at",params![Uuid::new_v4().to_string(),source.name,source.executable,relative,now()])?;
        Ok(relative)
    }
}

fn media_reference_key(relative: &str) -> String {
    relative.replace('\\', "/").to_lowercase()
}

fn media_references(connection: &Connection) -> Result<HashSet<String>> {
    let mut statement = connection.prepare(
        "SELECT content FROM clipboard_items WHERE kind='image'
         UNION ALL SELECT icon_file FROM clipboard_apps
         UNION ALL SELECT icon_file FROM file_type_icons
         UNION ALL SELECT image_rel_path FROM pin_records",
    )?;
    let mut references = HashSet::new();
    for relative in statement.query_map([], |row| row.get::<_, Option<String>>(0))? {
        if let Some(relative) = relative? {
            references.insert(media_reference_key(&relative));
        }
    }
    Ok(references)
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_media_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<ClipboardItem> {
    Ok(ClipboardItem {
        seq: row.get(0)?,
        id: row.get(1)?,
        kind: row.get(2)?,
        sub_kind: row.get(3)?,
        group_id: row.get(4)?,
        source_app_id: row.get(5)?,
        content: row.get(6)?,
        raw_payload: row.get(7)?,
        search_text: row.get(8)?,
        summary: row.get(9)?,
        size: row.get(10)?,
        width: row.get(11)?,
        height: row.get(12)?,
        use_count: row.get(13)?,
        is_favorite: row.get(14)?,
        is_pinned: row.get(15)?,
        is_sensitive: row.get(16)?,
        note: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
        source_app_name: row.get(20)?,
    })
}

fn query_filter(query: &ClipboardQuery) -> (String, Vec<Value>) {
    let mut filters = vec!["1=1".to_owned()];
    let mut values = Vec::new();
    if let Some(kind) = &query.kind {
        filters.push("i.kind=?".into());
        values.push(Value::Text(kind.clone()));
    }
    if query.favorites_only {
        filters.push("i.is_favorite=1".into());
    }
    if let Some(group) = &query.group_id {
        filters.push("i.group_id=?".into());
        values.push(Value::Text(group.clone()));
    }
    if !query.text.is_empty() {
        if query.text.chars().count() >= 3 {
            filters.push(
                "i.seq IN(SELECT rowid FROM clipboard_items_fts WHERE clipboard_items_fts MATCH ?)"
                    .into(),
            );
            values.push(Value::Text(format!(
                "\"{}\"",
                query.text.replace('"', "\"\"")
            )));
        } else {
            filters.push("(instr(lower(coalesce(i.search_text,'')),lower(?))>0 OR instr(lower(coalesce(i.note,'')),lower(?))>0)".into());
            values.extend([
                Value::Text(query.text.clone()),
                Value::Text(query.text.clone()),
            ]);
        }
    }
    (filters.join(" AND "), values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_stay_out_of_new_history_until_collection_is_enabled() {
        let data_dir =
            std::env::temp_dir().join(format!("orbment-clipboard-secret-{}", Uuid::new_v4()));
        let mut config = ClipboardConfig::default();
        let sample = "api_key=synthetic-test-value";
        let store = ClipboardStore::open(&data_dir, &config).unwrap();
        assert!(store.insert_text(sample).unwrap().is_none());
        assert_eq!(store.count(&ClipboardQuery::default()).unwrap(), 0);
        drop(store);

        config.sensitive.collect_secrets = true;
        let store = ClipboardStore::open(&data_dir, &config).unwrap();
        let seq = store.insert_text(sample).unwrap().unwrap();
        let item = store.get(seq).unwrap().unwrap();
        assert!(item.is_sensitive);
        assert_eq!(item.content, sample);
        drop(store);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn queries_read_committed_wal_data_while_the_writer_is_busy() {
        let data_dir =
            std::env::temp_dir().join(format!("orbment-clipboard-wal-{}", Uuid::new_v4()));
        let store = ClipboardStore::open(&data_dir, &ClipboardConfig::default()).unwrap();
        let seq = store.insert_text("committed text").unwrap().unwrap();
        let group = store.create_group("group", "").unwrap();
        let mut writer = store.write_connection().unwrap();
        let transaction = writer.transaction().unwrap();
        transaction
            .execute(
                "UPDATE clipboard_items SET content='uncommitted text' WHERE seq=?1",
                [seq],
            )
            .unwrap();
        let reader_store = store.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let result = (|| -> Result<_> {
                Ok((
                    reader_store.get(seq)?.unwrap().content,
                    reader_store.latest()?.unwrap().seq,
                    reader_store.query(&ClipboardQuery::default())?.len(),
                    reader_store.count(&ClipboardQuery::default())?,
                    reader_store
                        .groups()?
                        .into_iter()
                        .map(|g| g.id)
                        .collect::<Vec<_>>(),
                    reader_store.source_icon("unknown.exe")?,
                    reader_store.source_excluded("unknown.exe", &["excluded".into()])?,
                ))
            })();
            let _ = sender.send(result);
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(5));
        // Always release the writer before joining, even if a shared-lock regression times out.
        transaction.commit().unwrap();
        drop(writer);
        reader.join().unwrap();
        let committed = store.get(seq).unwrap().unwrap().content;
        assert!(
            store
                .read_connection()
                .unwrap()
                .execute("DELETE FROM clipboard_items", [])
                .is_err()
        );
        drop(store);
        std::fs::remove_dir_all(data_dir).unwrap();
        assert_eq!(
            result.unwrap().unwrap(),
            (
                "committed text".into(),
                seq,
                1,
                1,
                vec![group.id],
                None,
                false
            )
        );
        assert_eq!(committed, "uncommitted text");
    }

    fn media_store() -> ClipboardStore {
        let mut connection = Connection::open_in_memory().unwrap();
        db::initialize(&mut connection).unwrap();
        let connection = Arc::new(Mutex::new(connection));
        let mut config = ClipboardConfig::default();
        config.content.delete_confirm = false;
        ClipboardStore {
            write_connection: connection.clone(),
            read_connection: connection,
            data_dir: std::env::temp_dir()
                .join(format!("orbment-clipboard-media-{}", Uuid::new_v4())),
            config: Arc::new(config),
            on_change: Arc::new(Mutex::new(None)),
        }
    }

    fn write_test_media(store: &ClipboardStore, folder: &str, name: &str) -> PathBuf {
        let directory = store.data_dir.join("media").join(folder);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(name);
        std::fs::write(&path, b"image").unwrap();
        path
    }

    fn insert_test_image(store: &ClipboardStore, relative: &str) -> i64 {
        store
            .save_capture(Captured {
                kind: "image".into(),
                sub_kind: None,
                content: relative.into(),
                raw_payload: None,
                search_text: String::new(),
                size: 5,
                dimensions: None,
                source: None,
            })
            .unwrap()
            .unwrap()
    }

    #[test]
    fn png_capture_returns_the_committed_sequence_and_reuses_media_and_ocr() {
        let store = media_store();
        let bytes = b"captured image";
        let seq = store
            .save_png_capture(bytes, (12, 34), None)
            .unwrap()
            .unwrap();
        let item = store.get(seq).unwrap().unwrap();
        assert_eq!(item.kind, "image");
        assert_eq!(item.content, image_relative_path(bytes));
        assert_eq!((item.width, item.height), (Some(12), Some(34)));
        assert_eq!(item.size, Some(bytes.len() as i64));
        let path = store.data_dir.join(&item.content);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        store
            .set_ocr_text(seq, &item.content, "recognized text")
            .unwrap();

        assert_eq!(
            store.save_png_capture(bytes, (12, 34), None).unwrap(),
            Some(seq)
        );
        let reused = store.get(seq).unwrap().unwrap();
        assert_eq!(reused.use_count, item.use_count + 1);
        assert_eq!(reused.search_text.as_deref(), Some("recognized text"));
        assert_eq!(store.count(&ClipboardQuery::default()).unwrap(), 1);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }

    #[test]
    fn media_resolution_uses_native_separators_for_mixed_database_paths() {
        let store = media_store();
        write_test_media(&store, "clipboard_images", "existing.png");
        for name in ["existing.png", "missing.png"] {
            let expected = store
                .data_dir
                .join("media")
                .join("clipboard_images")
                .join(name);
            for relative in [
                format!("media/clipboard_images/{name}"),
                format!(r"media\clipboard_images\{name}"),
                format!(r"media/clipboard_images\{name}"),
                format!(r"media\clipboard_images/{name}"),
            ] {
                let resolved = store.resolve_media(&relative).unwrap();
                assert_eq!(resolved.as_os_str(), expected.as_os_str(), "{relative}");
            }
        }
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }

    #[test]
    fn media_resolution_rejects_absolute_and_traversal_paths() {
        let store = media_store();
        for relative in [
            "",
            "../outside.png",
            "media/../outside.png",
            r"media\..\outside.png",
            r"media/clipboard_images\..\..\outside.png",
            "/media/clipboard_images/image.png",
            r"\media\clipboard_images\image.png",
            "C:/media/clipboard_images/image.png",
            r"\\server\media\clipboard_images\image.png",
            "other/image.png",
        ] {
            assert!(store.resolve_media(relative).is_err(), "{relative}");
        }
        #[cfg(windows)]
        for relative in [
            "media/C:/outside.png",
            r"media\C:outside.png",
            r"media\clipboard_images/C:outside.png",
        ] {
            assert!(store.resolve_media(relative).is_err(), "{relative}");
        }
    }

    #[test]
    fn cleanup_preserves_normalized_references_from_every_media_table() {
        let store = media_store();
        let retained = [
            write_test_media(&store, "clipboard_images", "item.png"),
            write_test_media(&store, "clipboard_icons", "app.png"),
            write_test_media(&store, "clipboard_icons", "filetype.png"),
            write_test_media(&store, "clipboard_images", "pin.png"),
        ];
        let orphans = [
            write_test_media(&store, "clipboard_images", "orphan.png"),
            write_test_media(&store, "clipboard_icons", "orphan.png"),
        ];
        insert_test_image(&store, r"MEDIA\CLIPBOARD_IMAGES\ITEM.PNG");
        store
            .insert_text("media/clipboard_images/orphan.png")
            .unwrap();
        store
            .write_connection()
            .unwrap()
            .execute_batch(
                r"INSERT INTO clipboard_apps(id,name,icon_file,created_at,updated_at)
                  VALUES('app','app','MEDIA\CLIPBOARD_ICONS\APP.PNG','t','t'),
                        ('no-icon','no-icon',NULL,'t','t');
                  INSERT INTO file_type_icons(cache_key,icon_file,created_at,updated_at)
                  VALUES('png','media\clipboard_icons/FILETYPE.PNG','t','t');
                  INSERT INTO pin_records(id,pin_kind,image_rel_path,pos_x,pos_y,width,height,created_at)
                  VALUES('pin','image','media/clipboard_images\PIN.PNG',0,0,1,1,'t'),
                        ('text-pin','text',NULL,0,0,1,1,'t');",
            )
            .unwrap();

        let report = store.cleanup().unwrap();
        assert_eq!(report.deleted_items, 0);
        assert_eq!(report.deleted_media, orphans.len());
        for path in retained {
            assert!(path.is_file(), "{}", path.display());
        }
        for path in orphans {
            assert!(!path.exists(), "{}", path.display());
        }
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }

    #[test]
    fn delete_reclaims_only_affected_images_without_remaining_references() {
        let store = media_store();
        let disposable = write_test_media(&store, "clipboard_images", "delete.png");
        let retained = [
            write_test_media(&store, "clipboard_images", "shared.png"),
            write_test_media(&store, "clipboard_images", "app.png"),
            write_test_media(&store, "clipboard_images", "filetype.png"),
            write_test_media(&store, "clipboard_images", "pin.png"),
            write_test_media(&store, "clipboard_images", "orphan.png"),
            write_test_media(&store, "clipboard_icons", "orphan.png"),
            write_test_media(&store, "clipboard_images", "text.png"),
        ];
        let selected = [
            insert_test_image(&store, r"media\clipboard_images\delete.png"),
            insert_test_image(&store, "media/clipboard_images/delete.png"),
            insert_test_image(&store, r"media\clipboard_images/shared.png"),
            insert_test_image(&store, "media/clipboard_images/app.png"),
            insert_test_image(&store, "media/clipboard_images/filetype.png"),
            insert_test_image(&store, "media/clipboard_images/pin.png"),
            insert_test_image(&store, r"media\clipboard_images\missing.png"),
            store
                .insert_text("media/clipboard_images/text.png")
                .unwrap()
                .unwrap(),
        ];
        let shared = insert_test_image(&store, r"MEDIA/CLIPBOARD_IMAGES\SHARED.PNG");
        store
            .write_connection()
            .unwrap()
            .execute_batch(
                r"INSERT INTO clipboard_apps(id,name,icon_file,created_at,updated_at)
                  VALUES('app','app','MEDIA\CLIPBOARD_IMAGES\APP.PNG','t','t');
                  INSERT INTO file_type_icons(cache_key,icon_file,created_at,updated_at)
                  VALUES('png','media\clipboard_images/FILETYPE.PNG','t','t');
                  INSERT INTO pin_records(id,pin_kind,image_rel_path,pos_x,pos_y,width,height,created_at)
                  VALUES('pin','image','media/clipboard_images\PIN.PNG',0,0,1,1,'t');",
            )
            .unwrap();
        let mut batch = selected.to_vec();
        batch.push(selected[0]);
        assert_eq!(
            store.delete(&batch, DeleteOptions::default()).unwrap(),
            selected.len()
        );
        assert_eq!(store.delete(&[], DeleteOptions::default()).unwrap(), 0);
        assert_eq!(
            store.delete(&[i64::MAX], DeleteOptions::default()).unwrap(),
            0
        );
        for seq in selected {
            assert!(store.get(seq).unwrap().is_none());
        }
        assert!(store.get(shared).unwrap().is_some());
        assert!(!disposable.exists());
        for path in retained {
            assert!(path.is_file(), "{}", path.display());
        }
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }

    #[test]
    fn delete_rejects_unsafe_image_paths_before_committing_the_batch() {
        let store = media_store();
        let image = write_test_media(&store, "clipboard_images", "safe.png");
        let outside = store.data_dir.join("outside.png");
        std::fs::write(&outside, b"outside").unwrap();
        let batch = [
            insert_test_image(&store, "media/clipboard_images/safe.png"),
            insert_test_image(&store, r"media\..\outside.png"),
        ];
        assert!(store.delete(&batch, DeleteOptions::default()).is_err());
        for seq in batch {
            assert!(store.get(seq).unwrap().is_some());
        }
        assert!(image.is_file());
        assert_eq!(std::fs::read(outside).unwrap(), b"outside");
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }

    #[test]
    fn trigram_and_short_queries_search_content_and_notes() {
        let mut connection = Connection::open_in_memory().unwrap();
        db::initialize(&mut connection).unwrap();
        let connection = Arc::new(Mutex::new(connection));
        let store = ClipboardStore {
            write_connection: connection.clone(),
            read_connection: connection,
            data_dir: PathBuf::new(),
            config: Arc::new(ClipboardConfig::default()),
            on_change: Arc::new(Mutex::new(None)),
        };
        let seq = store.insert_text("alpha 中文内容").unwrap().unwrap();
        store.insert_text("unrelated").unwrap();
        store.set_note(seq, "备注 contains \"quoted\"").unwrap();
        for text in ["中文内", "中文", "中", "alpha", "备注", "\"quoted\""] {
            let matches = store
                .query(&ClipboardQuery {
                    text: text.into(),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                matches.iter().map(|item| item.seq).collect::<Vec<_>>(),
                [seq],
                "{text}"
            );
        }
    }

    #[test]
    fn listed_items_carry_their_source_application_name() {
        let mut connection = Connection::open_in_memory().unwrap();
        db::initialize(&mut connection).unwrap();
        let connection = Arc::new(Mutex::new(connection));
        let store = ClipboardStore {
            write_connection: connection.clone(),
            read_connection: connection,
            data_dir: PathBuf::new(),
            config: Arc::new(ClipboardConfig::default()),
            on_change: Arc::new(Mutex::new(None)),
        };
        let sourced = store.insert_text("#3B82F6").unwrap().unwrap();
        let unknown = store.insert_text("no source").unwrap().unwrap();
        {
            let connection = store.write_connection().unwrap();
            connection
                .execute(
                    "INSERT INTO clipboard_apps(id,name,bundle_or_exe,created_at,updated_at) VALUES('app-1','Figma','figma.exe','t','t')",
                    [],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE clipboard_items SET source_app_id='app-1' WHERE seq=?1",
                    [sourced],
                )
                .unwrap();
        }
        let names: std::collections::BTreeMap<i64, Option<String>> = store
            .query(&ClipboardQuery::default())
            .unwrap()
            .into_iter()
            .map(|item| (item.seq, item.source_app_name))
            .collect();
        assert_eq!(names[&sourced].as_deref(), Some("Figma"));
        assert_eq!(names[&unknown], None);
        assert_eq!(
            store
                .get(sourced)
                .unwrap()
                .unwrap()
                .source_app_name
                .as_deref(),
            Some("Figma")
        );
    }

    #[test]
    fn delete_checks_the_whole_batch_before_removing_any_items() {
        let mut connection = Connection::open_in_memory().unwrap();
        db::initialize(&mut connection).unwrap();
        let connection = Arc::new(Mutex::new(connection));
        let mut config = ClipboardConfig::default();
        config.content.delete_confirm = false;
        config.content.delete_favorite_items = true;
        config.content.delete_pinned_items = true;
        let store = ClipboardStore {
            write_connection: connection.clone(),
            read_connection: connection,
            data_dir: std::env::temp_dir().join(Uuid::new_v4().to_string()),
            config: Arc::new(config),
            on_change: Arc::new(Mutex::new(None)),
        };
        let image = write_test_media(&store, "clipboard_images", "ordinary.png");
        let plain = insert_test_image(&store, "media/clipboard_images/ordinary.png");
        let favorite = store.insert_text("favorite entry").unwrap().unwrap();
        let pinned = store.insert_text("pinned entry").unwrap().unwrap();
        store.set_favorite(favorite, true).unwrap();
        store.set_pinned(pinned, true).unwrap();
        let batch = [plain, favorite, pinned];
        for options in [
            DeleteOptions {
                confirmed: true,
                in_favorites_group: false,
            },
            DeleteOptions {
                confirmed: false,
                in_favorites_group: true,
            },
        ] {
            assert!(store.delete(&batch, options).is_err());
            assert!(image.is_file());
            for seq in batch {
                assert!(store.get(seq).unwrap().is_some());
            }
        }
        assert_eq!(
            store
                .delete(
                    &batch,
                    DeleteOptions {
                        confirmed: true,
                        in_favorites_group: true,
                    },
                )
                .unwrap(),
            batch.len()
        );
        for seq in batch {
            assert!(store.get(seq).unwrap().is_none());
        }
        assert!(!image.exists());
        std::fs::remove_dir_all(&store.data_dir).unwrap();
    }
}
