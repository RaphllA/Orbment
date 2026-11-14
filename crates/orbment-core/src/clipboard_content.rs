//! Format-independent clipboard rules shared by live capture and the standalone importer.
use anyhow::{Result, ensure};
use regex::Regex;
use std::sync::LazyLock;

pub const SEARCH_TEXT_LIMIT: usize = 64 * 1024;

pub fn truncate_search_text(text: &str) -> String {
    let mut end = text.len().min(SEARCH_TEXT_LIMIT);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

pub fn is_sensitive(text: &str) -> bool {
    static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
        r"(?im)(-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----|",
        r"\b(?:sk-(?:proj-)?[A-Za-z0-9_-]{16,}|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[A-Z0-9]{16}|xox[baprs]-[A-Za-z0-9-]{16,})\b|",
        r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b|",
        r#"\b(?:password|passwd|pwd|api[_-]?key|access[_-]?token|client[_-]?secret|authorization)[\"']?\s*[=:]\s*[\"']?\S{4,})"#
    )).expect("static clipboard secret expression")
    });
    SECRETS.is_match(text)
}

/// Deduplication only: text is verbatim plain content, images use their relative PNG path,
/// and files use a compact JSON array preserving the original path order.
pub fn content_hash(kind: &str, canonical_content: &str) -> String {
    blake3::hash(format!("{kind}:{canonical_content}").as_bytes())
        .to_hex()
        .to_string()
}

/// Content addressing is restricted to clipboard image filenames.
pub fn image_relative_path(png_bytes: &[u8]) -> String {
    format!(
        "media/clipboard_images/{}.png",
        blake3::hash(png_bytes).to_hex()
    )
}

pub fn html_plain(raw: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(raw);
    let offset = |key: &str| -> Option<usize> {
        text.lines()
            .find_map(|line| line.strip_prefix(key).and_then(|v| v.trim().parse().ok()))
    };
    let fragment = match (offset("StartFragment:"), offset("EndFragment:")) {
        (Some(start), Some(end)) if start <= end && end <= raw.len() => &raw[start..end],
        _ => {
            let start = raw.iter().position(|byte| *byte == b'<').unwrap_or(0);
            &raw[start..]
        }
    };
    Ok(html2text::from_read(fragment, 160)?.trim().to_owned())
}

/// RTF Unicode fallbacks, legacy code pages and destination groups affect searchable text.
pub fn rtf_plain(raw: &[u8]) -> Result<String> {
    ensure!(
        raw.starts_with(b"{\\rtf"),
        "{}",
        crate::i18n::tr("clipboard.error.invalid_content")
    );
    #[derive(Clone, Copy)]
    struct State {
        skip: bool,
        uc: usize,
        encoding: &'static encoding_rs::Encoding,
    }
    let mut state = State {
        skip: false,
        uc: 1,
        encoding: encoding_rs::WINDOWS_1252,
    };
    let mut stack = Vec::new();
    let mut output = Vec::<u16>::new();
    let mut pending = Vec::new();
    let flush =
        |bytes: &mut Vec<u8>, output: &mut Vec<u16>, encoding: &'static encoding_rs::Encoding| {
            if !bytes.is_empty() {
                let (decoded, _, _) = encoding.decode(bytes);
                output.extend(decoded.encode_utf16());
                bytes.clear();
            }
        };
    let mut i = 0;
    let mut fallback = 0;
    while i < raw.len() {
        let byte = raw[i];
        i += 1;
        match byte {
            b'{' => {
                flush(&mut pending, &mut output, state.encoding);
                stack.push(state);
            }
            b'}' => {
                flush(&mut pending, &mut output, state.encoding);
                state = stack.pop().unwrap_or(State {
                    skip: true,
                    uc: 1,
                    encoding: encoding_rs::WINDOWS_1252,
                });
            }
            b'\\' if i < raw.len() => {
                let c = raw[i];
                i += 1;
                if matches!(c, b'\\' | b'{' | b'}') {
                    if fallback > 0 {
                        fallback -= 1;
                    } else if !state.skip {
                        pending.push(c);
                    }
                } else if c == b'*' {
                    flush(&mut pending, &mut output, state.encoding);
                    state.skip = true;
                } else if c == b'\'' && i + 2 <= raw.len() {
                    if let Ok(hex) = std::str::from_utf8(&raw[i..i + 2])
                        && let Ok(value) = u8::from_str_radix(hex, 16)
                    {
                        if fallback > 0 {
                            fallback -= 1;
                        } else if !state.skip {
                            pending.push(value);
                        }
                    }
                    i += 2;
                } else if c.is_ascii_alphabetic() {
                    flush(&mut pending, &mut output, state.encoding);
                    let start = i - 1;
                    while i < raw.len() && raw[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    let word = &raw[start..i];
                    let number_start = i;
                    if raw.get(i) == Some(&b'-') {
                        i += 1;
                    }
                    while i < raw.len() && raw[i].is_ascii_digit() {
                        i += 1;
                    }
                    let value = std::str::from_utf8(&raw[number_start..i])
                        .ok()
                        .and_then(|s| s.parse::<i32>().ok());
                    if raw.get(i) == Some(&b' ') {
                        i += 1;
                    }
                    match word {
                        b"fonttbl" | b"colortbl" | b"stylesheet" | b"info" | b"pict"
                        | b"object" | b"header" | b"footer" | b"fldinst" | b"datafield" => {
                            state.skip = true
                        }
                        b"ansicpg" => {
                            let page = value.unwrap_or(1252);
                            let label = match page {
                                65001 => "utf-8".to_owned(),
                                932 => "shift_jis".to_owned(),
                                936 => "gbk".to_owned(),
                                949 => "euc-kr".to_owned(),
                                950 => "big5".to_owned(),
                                10000 => "macintosh".to_owned(),
                                _ => format!("windows-{page}"),
                            };
                            state.encoding = encoding_rs::Encoding::for_label(label.as_bytes())
                                .ok_or_else(|| {
                                    anyhow::anyhow!(crate::i18n::tr(
                                        "clipboard.error.invalid_content"
                                    ))
                                })?;
                        }
                        b"uc" => state.uc = value.unwrap_or(1).max(0) as usize,
                        b"bin" => {
                            i = i
                                .saturating_add(value.unwrap_or(0).max(0) as usize)
                                .min(raw.len());
                        }
                        b"u" => {
                            if !state.skip {
                                output.push(value.unwrap_or(0) as u16);
                            }
                            fallback = state.uc;
                        }
                        b"par" | b"line" if !state.skip => output.push(b'\n' as u16),
                        b"tab" if !state.skip => output.push(b'\t' as u16),
                        b"emdash" if !state.skip => output.push(0x2014),
                        b"endash" if !state.skip => output.push(0x2013),
                        b"bullet" if !state.skip => output.push(0x2022),
                        _ => {}
                    }
                } else if !state.skip && c == b'~' {
                    flush(&mut pending, &mut output, state.encoding);
                    output.push(0xa0);
                }
            }
            b'\r' | b'\n' | 0 => {}
            _ => {
                if fallback > 0 {
                    fallback -= 1;
                } else if !state.skip {
                    pending.push(byte);
                }
            }
        }
    }
    flush(&mut pending, &mut output, state.encoding);
    Ok(String::from_utf16_lossy(&output).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_limit_preserves_utf8_boundary() {
        let value = format!("{}中文", "a".repeat(SEARCH_TEXT_LIMIT - 2));
        assert_eq!(truncate_search_text(&value).len(), SEARCH_TEXT_LIMIT - 2);
    }

    #[test]
    fn recognizes_secret_formats_without_flagging_ordinary_prose() {
        for value in [
            "-----BEGIN RSA PRIVATE KEY-----",
            "api_key=abcdefghijklmno",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature",
            "ghp_123456789012345678901234",
        ] {
            assert!(is_sensitive(value));
        }
        assert!(!is_sensitive("A password manager protects your accounts."));
    }
}
