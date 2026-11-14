//! Event-driven clipboard capture, persistence and native write/paste operations.
mod content;
mod listener;
mod native;
mod paste;
mod store;
mod types;

pub use content::{display_text, is_sensitive, truncate_search_text};
pub use listener::ClipboardService;
pub use orbment_core::config::ClipboardConfig;
pub use paste::paste_key_sequence;
pub use store::ClipboardStore;
pub use types::{
    CleanupReport, ClipboardGroup, ClipboardItem, ClipboardQuery, DeleteOptions, WriteContent,
    WriteOptions,
};
