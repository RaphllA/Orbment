use windows::Win32::{Foundation::SYSTEMTIME, System::SystemInformation::GetLocalTime};

/// Capture-time metadata, rather than export-time foreground-window metadata.
pub struct SaveTemplateContext<'a> {
    pub title: &'a str,
    pub process: &'a str,
    pub counter: u64,
    pub now: SYSTEMTIME,
}

impl<'a> SaveTemplateContext<'a> {
    pub fn now(title: &'a str, counter: u64) -> Self {
        Self {
            title,
            process: "",
            counter,
            now: unsafe { GetLocalTime() },
        }
    }
}

/// Expand capture-time metadata for filenames, text marks and watermarks.
pub fn expand_text(template: &str, context: &SaveTemplateContext<'_>) -> String {
    let mut output = String::new();
    let mut rest = template;
    while let Some(index) = rest.find(['$', '%']) {
        output.push_str(&rest[..index]);
        rest = &rest[index..];
        let delimiter = rest.as_bytes()[0] as char;
        let Some(end) = rest[1..].find(delimiter).map(|position| position + 1) else {
            break;
        };
        let variable = &rest[1..end];
        if delimiter == '$' {
            output.push_str(&date_pattern(variable, &context.now));
        } else if let Some(value) = metadata(variable, context) {
            output.push_str(&value);
        } else {
            output.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    output.push_str(rest);
    output
}

/// Produce one legal Windows filename from the expanded capture metadata.
pub fn expand(template: &str, context: &SaveTemplateContext<'_>) -> String {
    let output = expand_text(template, context);
    let filename: String = output
        .chars()
        .map(|character| {
            if character.is_control() || "<>:\"/\\|?*".contains(character) {
                '_'
            } else {
                character
            }
        })
        .collect();
    let filename = filename.trim_end_matches([' ', '.']);
    if filename.is_empty() {
        return "Orbment.png".into();
    }
    // Device names remain reserved even when followed by an extension.
    let stem = filename
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    if reserved {
        format!("_{filename}")
    } else {
        filename.into()
    }
}

fn metadata(variable: &str, context: &SaveTemplateContext<'_>) -> Option<String> {
    let (name, limit) = match variable.rsplit_once('#') {
        Some((name, count)) => (name, Some(count.parse::<usize>().ok()?)),
        None => (variable, None),
    };
    let strip_extension = name.ends_with('!');
    let name = name.trim_end_matches('!');
    let value = match name {
        "title" => context.title.to_owned(),
        "process" => {
            let process = context
                .process
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or_default();
            if strip_extension {
                process
                    .rsplit_once('.')
                    .map_or(process, |(stem, _)| stem)
                    .to_owned()
            } else {
                process.to_owned()
            }
        }
        "counter" => context.counter.to_string(),
        _ => return None,
    };
    Some(match limit {
        Some(limit) => value.chars().take(limit).collect(),
        None => value,
    })
}

fn date_pattern(mut pattern: &str, time: &SYSTEMTIME) -> String {
    let fields = [
        ("yyyy", format!("{:04}", time.wYear)),
        ("zzz", format!("{:03}", time.wMilliseconds)),
        ("yy", format!("{:02}", time.wYear % 100)),
        ("MM", format!("{:02}", time.wMonth)),
        ("dd", format!("{:02}", time.wDay)),
        ("HH", format!("{:02}", time.wHour)),
        ("hh", format!("{:02}", (time.wHour + 11) % 12 + 1)),
        ("mm", format!("{:02}", time.wMinute)),
        ("ss", format!("{:02}", time.wSecond)),
        ("M", time.wMonth.to_string()),
        ("d", time.wDay.to_string()),
        ("H", time.wHour.to_string()),
        ("m", time.wMinute.to_string()),
        ("s", time.wSecond.to_string()),
    ];
    let mut output = String::new();
    while !pattern.is_empty() {
        if let Some((token, value)) = fields.iter().find(|(token, _)| pattern.starts_with(token)) {
            output.push_str(value);
            pattern = &pattern[token.len()..];
        } else {
            let character = pattern.chars().next().unwrap();
            output.push(character);
            pattern = &pattern[character.len_utf8()..];
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_metadata_and_date_expand_without_splitting_unicode() {
        let context = SaveTemplateContext {
            title: "一二三四五六七八九十十一",
            process: r"C:\Apps\editor.exe",
            counter: 7,
            now: SYSTEMTIME {
                wYear: 2026,
                wMonth: 10,
                wDay: 21,
                wHour: 9,
                wMinute: 8,
                wSecond: 3,
                ..Default::default()
            },
        };
        assert_eq!(
            expand(
                "Orbment_$yyyy-MM-dd_HH-mm-ss$_%title#10%_%process!%_%counter%.png",
                &context
            ),
            "Orbment_2026-10-21_09-08-03_一二三四五六七八九十_editor_7.png"
        );
        assert_eq!(expand("%title#0%CON.png", &context), "_CON.png");
        assert_eq!(expand("%unknown%_$yyyy", &context), "%unknown%_$yyyy");
    }
}
