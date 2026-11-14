use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub seq: i64,
    pub id: String,
    pub kind: String,
    pub sub_kind: Option<String>,
    pub group_id: Option<String>,
    pub source_app_id: Option<String>,
    pub content: String,
    pub raw_payload: Option<Vec<u8>>,
    pub search_text: Option<String>,
    pub summary: Option<String>,
    pub size: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub use_count: i64,
    pub is_favorite: bool,
    pub is_pinned: bool,
    pub is_sensitive: bool,
    pub note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Display name joined from `clipboard_apps`; `None` when the source is unknown.
    pub source_app_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClipboardGroup {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub is_hidden: bool,
    pub sort_order: i64,
}

#[derive(Clone, Debug)]
pub struct ClipboardQuery {
    pub text: String,
    pub kind: Option<String>,
    pub favorites_only: bool,
    pub group_id: Option<String>,
    pub offset: u32,
    pub limit: u32,
}
impl Default for ClipboardQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            kind: None,
            favorites_only: false,
            group_id: None,
            offset: 0,
            limit: 100,
        }
    }
}

#[derive(Clone, Debug)]
pub enum WriteContent {
    Text(String),
    Html { plain: String, raw: Vec<u8> },
    Rtf { plain: String, raw: Vec<u8> },
    Image(PathBuf),
    Files(Vec<PathBuf>),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOptions {
    pub plain_text: bool,
    pub files_as_paths: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DeleteOptions {
    pub confirmed: bool,
    pub in_favorites_group: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CleanupReport {
    pub deleted_items: usize,
    pub deleted_media: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Captured {
    pub kind: String,
    pub sub_kind: Option<String>,
    pub content: String,
    pub raw_payload: Option<Vec<u8>>,
    pub search_text: String,
    pub size: u64,
    pub dimensions: Option<(u32, u32)>,
    pub source: Option<SourceApp>,
}
#[derive(Clone, Debug)]
pub(crate) struct SourceApp {
    pub executable: String,
    pub name: String,
    pub icon_file: Option<String>,
}
