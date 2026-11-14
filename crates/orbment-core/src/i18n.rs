use crate::config::{QuickAskProvider, ThinkingEffort};
use anyhow::Result;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{LazyLock, RwLock},
};
use toml_edit::{DocumentMut, Item};

const BUILTIN: &str = include_str!("../../../assets/locales/zh-CN.toml");
static STRINGS: LazyLock<RwLock<BTreeMap<String, String>>> =
    LazyLock::new(|| RwLock::new(defaults()));

fn flatten(prefix: &str, table: &dyn toml_edit::TableLike, values: &mut BTreeMap<String, String>) {
    for (key, item) in table.iter() {
        let key = if prefix.is_empty() {
            key.to_owned()
        } else {
            format!("{prefix}.{key}")
        };
        if let Some(value) = item.as_str() {
            values.insert(key, value.to_owned());
        } else if let Some(table) = Item::as_table_like(item) {
            flatten(&key, table, values);
        }
    }
}
fn defaults() -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    flatten(
        "",
        BUILTIN
            .parse::<DocumentMut>()
            .expect("bundled locale")
            .as_table(),
        &mut values,
    );
    values
}
pub fn load(override_path: &Path) -> Result<()> {
    let mut values = defaults();
    match std::fs::read_to_string(override_path) {
        Ok(text) => flatten("", text.parse::<DocumentMut>()?.as_table(), &mut values),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    *STRINGS.write().expect("locale lock") = values;
    Ok(())
}
pub fn tr(key: &str) -> String {
    STRINGS
        .read()
        .expect("locale lock")
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

pub fn quick_ask_provider_label(provider: QuickAskProvider) -> String {
    tr(match provider {
        QuickAskProvider::OpenAI => "settings.option.OpenAI",
        QuickAskProvider::OpenAiCompatible => "settings.option.OpenAiCompatible",
        QuickAskProvider::Anthropic => "settings.option.Anthropic",
        QuickAskProvider::Google => "settings.option.Google",
    })
}

pub fn quick_ask_default_channel_name(provider: QuickAskProvider) -> String {
    tr(match provider {
        QuickAskProvider::OpenAI => "quick_ask.channel_openai",
        QuickAskProvider::OpenAiCompatible => "quick_ask.channel_openai_compatible",
        QuickAskProvider::Anthropic => "quick_ask.channel_anthropic",
        QuickAskProvider::Google => "quick_ask.channel_google",
    })
}

pub fn thinking_effort_label(effort: ThinkingEffort) -> String {
    tr(match effort {
        ThinkingEffort::Off => "settings.option.Off",
        ThinkingEffort::Low => "settings.option.Low",
        ThinkingEffort::Medium => "settings.option.Medium",
        ThinkingEffort::High => "settings.option.High",
        ThinkingEffort::Max => "settings.option.Max",
    })
}
