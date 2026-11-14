use anyhow::{Context, Result, ensure};
use orbment_core::i18n::tr;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Monitor skin display-item order, retaining the ignored motherboard slot.
pub const METRICS: &[(&str, u32)] = &[
    ("up", 1),
    ("down", 2),
    ("cpu", 4),
    ("memory", 8),
    ("gpu", 16),
    ("cpu_temperature", 32),
    ("gpu_temperature", 64),
    ("disk_temperature", 128),
    ("disk", 512),
    ("total_speed", 1024),
    ("cpu_frequency", 2048),
    ("today_traffic", 4096),
    ("disk_read", 8192),
    ("disk_write", 16384),
];

fn metric(name: &str) -> Option<&'static str> {
    let name = match name {
        "hdd" => "disk",
        "hdd_temperature" => "disk_temperature",
        "cpu_freq" => "cpu_frequency",
        other => other,
    };
    METRICS
        .iter()
        .find_map(|(key, _)| (*key == name).then_some(*key))
}

#[derive(Clone, Debug)]
pub struct SkinItem {
    pub metric: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub align: u8,
    pub show: bool,
}
#[derive(Clone, Debug)]
pub struct SkinLayout {
    pub width: i32,
    pub height: i32,
    pub items: Vec<SkinItem>,
}
#[derive(Clone, Debug)]
pub struct SkinFont {
    pub name: String,
    pub size: f32,
    pub style: u32,
}
#[derive(Clone, Debug)]
pub struct Skin {
    pub small: SkinLayout,
    pub large: SkinLayout,
    pub text_height: i32,
    pub no_label: bool,
    pub font: Option<SkinFont>,
    pub labels: BTreeMap<String, String>,
    pub text_colors: Vec<u32>,
    pub specify_each_item_color: bool,
    pub background_small: Option<PathBuf>,
    pub background_large: Option<PathBuf>,
}

impl Default for Skin {
    fn default() -> Self {
        let items = ["up", "down", "cpu", "memory"]
            .into_iter()
            .enumerate()
            .map(|(i, key)| SkinItem {
                metric: key.into(),
                x: 6 + (i % 2) as i32 * 114,
                y: 2 + (i / 2) as i32 * 21,
                width: 108,
                align: 0,
                show: true,
            })
            .collect::<Vec<_>>();
        Self {
            small: SkinLayout {
                width: 234,
                height: 26,
                items: items[..2].to_vec(),
            },
            large: SkinLayout {
                width: 234,
                height: 47,
                items,
            },
            text_height: 21,
            no_label: false,
            font: None,
            labels: BTreeMap::new(),
            text_colors: Vec::new(),
            specify_each_item_color: false,
            background_small: None,
            background_large: None,
        }
    }
}

impl Skin {
    pub fn load(directory: &Path) -> Result<Self> {
        let xml = directory.join("skin.xml");
        let file = if xml.exists() {
            xml
        } else {
            directory.join("skin.ini")
        };
        let source = decode_text(
            &std::fs::read(&file)
                .with_context(|| format!("{}: {}", tr("monitor-skin-read"), file.display()))?,
        )?;
        let mut skin = if file.extension().is_some_and(|ext| ext == "xml") {
            Self::parse_xml(&source)?
        } else {
            Self::parse_ini(&source)?
        };
        let background = |suffix| {
            [
                format!("background_{suffix}.png"),
                format!("background_{suffix}.bmp"),
            ]
            .into_iter()
            .map(|name| directory.join(name))
            .find(|path| path.is_file())
        };
        skin.background_small = background("s");
        skin.background_large = background("l");
        Ok(skin)
    }

    pub fn layout(&self, large: bool) -> &SkinLayout {
        if large { &self.large } else { &self.small }
    }
    pub fn display_mask(&self, large: bool) -> u32 {
        self.layout(large)
            .items
            .iter()
            .filter(|item| item.show)
            .fold(0, |mask, item| {
                mask | METRICS
                    .iter()
                    .find(|(key, _)| *key == item.metric)
                    .map_or(0, |(_, bit)| *bit)
            })
    }

    pub fn parse_ini(source: &str) -> Result<Self> {
        let mut values = BTreeMap::new();
        let mut section = String::new();
        for line in source.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                values.insert(
                    (section.clone(), key.trim().to_ascii_lowercase()),
                    value.trim().trim_matches('"').to_owned(),
                );
            }
        }
        let get = |section: &str, key: &str| {
            values
                .get(&(section.into(), key.into()))
                .map(String::as_str)
        };
        let number = |section, key: &str, default: i32| -> Result<i32> {
            get(section, key).map_or(Ok(default), |value| {
                value
                    .parse()
                    .with_context(|| format!("{}: {key}", tr("monitor-skin-number")))
            })
        };
        let boolean =
            |section, key: &str, default: bool| get(section, key).map_or(default, parse_bool);
        let mut skin = Self {
            text_height: number("layout", "text_height", 20)?,
            no_label: boolean("layout", "no_text", false),
            specify_each_item_color: boolean("skin", "specify_each_item_color", false),
            ..Self::default()
        };
        if let Some(color) = get("skin", "text_color") {
            skin.text_colors = colors(color)?;
        }
        if let Some(name) = get("skin", "font_name") {
            skin.font = Some(SkinFont {
                name: name.into(),
                size: number("skin", "font_size", 10)? as f32,
                style: number("skin", "font_style", 0)? as u32,
            });
        }
        for (key, _) in METRICS {
            if let Some(value) = get("skin", &format!("{key}_string")) {
                skin.labels.insert((*key).into(), value.into());
            }
        }
        for (suffix, layout) in [("s", &mut skin.small), ("l", &mut skin.large)] {
            layout.width = number("layout", &format!("width_{suffix}"), 220)?;
            layout.height = number(
                "layout",
                &format!("height_{suffix}"),
                if suffix == "s" { 28 } else { 43 },
            )?;
            for item in &mut layout.items {
                let key = &item.metric;
                item.x = number("layout", &format!("{key}_x_{suffix}"), item.x)?;
                item.y = number(
                    "layout",
                    &format!("{key}_y_{suffix}"),
                    if suffix == "s" { 4 } else { item.y },
                )?;
                item.width = number(
                    "layout",
                    &format!("{key}_width_{suffix}"),
                    if matches!(key.as_str(), "down" | "memory") {
                        110
                    } else {
                        108
                    },
                )?;
                item.align = number("layout", &format!("{key}_align_{suffix}"), 0)? as u8;
                item.show = boolean("layout", &format!("show_{key}_{suffix}"), true);
            }
            // Legacy skins may explicitly place CPU/memory even in their small layout.
            if suffix == "s" {
                for key in ["cpu", "memory"] {
                    if boolean("layout", &format!("show_{key}_s"), false) {
                        layout.items.push(SkinItem {
                            metric: key.into(),
                            x: number("layout", &format!("{key}_x_s"), 0)?,
                            y: number("layout", &format!("{key}_y_s"), 0)?,
                            width: number("layout", &format!("{key}_width_s"), 0)?,
                            align: number("layout", &format!("{key}_align_s"), 0)? as u8,
                            show: true,
                        });
                    }
                }
            }
        }
        skin.validate()?;
        Ok(skin)
    }

    pub fn parse_xml(source: &str) -> Result<Self> {
        let document = roxmltree::Document::parse(source).context(tr("monitor-skin-xml"))?;
        let root = document.root_element();
        let mut skin = Self::default();
        for section in root.children().filter(|node| node.is_element()) {
            match section.tag_name().name() {
                "skin" => {
                    for node in section.children().filter(|node| node.is_element()) {
                        let text = node.text().unwrap_or_default();
                        match node.tag_name().name() {
                            "text_color" => skin.text_colors = colors(text)?,
                            "specify_each_item_color" => {
                                skin.specify_each_item_color = parse_bool(text)
                            }
                            "font" => {
                                skin.font = Some(SkinFont {
                                    name: node.attribute("name").unwrap_or_default().into(),
                                    size: attribute(node, "size", 10)? as f32,
                                    style: attribute(node, "style", 0)? as u32,
                                })
                            }
                            "display_text" => {
                                for label in node.children().filter(|node| node.is_element()) {
                                    if let Some(key) = metric(label.tag_name().name()) {
                                        skin.labels.insert(
                                            key.into(),
                                            label.text().unwrap_or_default().into(),
                                        );
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "layout" => {
                    skin.text_height = attribute(section, "text_height", 20)?;
                    skin.no_label = section.attribute("no_label").is_some_and(parse_bool);
                    for node in section.children().filter(|node| node.is_element()) {
                        let target = match node.tag_name().name() {
                            "layout_s" => &mut skin.small,
                            "layout_l" => &mut skin.large,
                            _ => continue,
                        };
                        target.width = attribute(node, "width", target.width)?;
                        target.height = attribute(node, "height", target.height)?;
                        target.items.clear();
                        for item in node.children().filter(|node| node.is_element()) {
                            if let Some(key) = metric(item.tag_name().name()) {
                                target.items.push(SkinItem {
                                    metric: key.into(),
                                    x: attribute(item, "x", 0)?,
                                    y: attribute(item, "y", 0)?,
                                    width: attribute(item, "width", 0)?,
                                    align: attribute(item, "align", 0)? as u8,
                                    show: item.attribute("show").is_some_and(parse_bool),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        skin.validate()?;
        Ok(skin)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=4096).contains(&self.text_height),
            "{}",
            tr("monitor-skin-dimensions")
        );
        for layout in [&self.small, &self.large] {
            ensure!(
                (1..=16384).contains(&layout.width) && (1..=16384).contains(&layout.height),
                "{}",
                tr("monitor-skin-dimensions")
            );
            for item in &layout.items {
                ensure!(
                    item.width >= 0 && item.align <= 3,
                    "{}",
                    tr("monitor-skin-dimensions")
                );
            }
        }
        if let Some(font) = &self.font {
            ensure!(
                font.size > 0.0 && font.size <= 1024.0,
                "{}",
                tr("monitor-skin-font")
            );
        }
        Ok(())
    }
}

fn attribute(node: roxmltree::Node<'_, '_>, key: &str, default: i32) -> Result<i32> {
    node.attribute(key).map_or(Ok(default), |value| {
        value
            .parse()
            .with_context(|| format!("{}: {key}", tr("monitor-skin-number")))
    })
}
fn parse_bool(value: &str) -> bool {
    value == "1" || value.eq_ignore_ascii_case("true")
}
fn colors(value: &str) -> Result<Vec<u32>> {
    value
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<i64>()
                .map(|value| value as u32)
                .context(tr("monitor-skin-color"))
        })
        .collect()
}
fn decode_text(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(
            bytes.len().is_multiple_of(2),
            "{}",
            tr("monitor-skin-encoding")
        );
        let little = bytes[0] == 0xff;
        let units = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| {
                if little {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect::<Vec<_>>();
        return String::from_utf16(&units).context(tr("monitor-skin-encoding"));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.into());
    }
    // Older monitor INI skins also use the Windows ANSI codepage.
    use windows::Win32::Globalization::{CP_ACP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};
    let length = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, None) };
    ensure!(length > 0, "{}", tr("monitor-skin-encoding"));
    let mut units = vec![0u16; length as usize];
    ensure!(
        unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, Some(&mut units)) }
            == length,
        "{}",
        tr("monitor-skin-encoding")
    );
    String::from_utf16(&units).context(tr("monitor-skin-encoding"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_aliases_entities_and_plugin_exclusion() {
        let skin = Skin::parse_xml(r#"<root><skin><display_text><up>A &amp; B</up></display_text></skin><layout text_height="18"><layout_s width="200" height="30"><hdd_temperature x="1" y="2" width="80" align="3" show="true"/><plugin x="0"/></layout_s></layout></root>"#).unwrap();
        assert_eq!(skin.small.items.len(), 1);
        assert_eq!(skin.small.items[0].metric, "disk_temperature");
        assert_eq!(skin.labels["up"], "A & B");
        assert_eq!(skin.display_mask(false), 128);
    }
    #[test]
    fn ini_sections_quotes_layout_and_utf16() {
        let source = "[skin]\nup_string=\"Upload: \"\nfont_name=\"Test\"\n[layout]\nwidth_s=300\nshow_down_s=0\nup_align_s=3";
        let skin = Skin::parse_ini(source).unwrap();
        assert_eq!(skin.labels["up"], "Upload: ");
        assert_eq!(skin.small.width, 300);
        assert_eq!(skin.display_mask(false), 1);
        let bytes = [
            vec![0xff, 0xfe],
            source.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ]
        .concat();
        assert_eq!(decode_text(&bytes).unwrap(), source);
        assert!(Skin::parse_ini("[layout]\nwidth_s=-1").is_err());
    }
}
