pub use orbment_core::clipboard_content::{
    html_plain, is_sensitive, rtf_plain, truncate_search_text,
};

/// Redaction is a presentation boundary; the database and clipboard retain the original.
pub fn display_text(text: &str, sensitive: bool, redact: bool) -> String {
    if sensitive && redact {
        "••••••••".to_owned()
    } else {
        text.to_owned()
    }
}

pub(crate) fn sub_kind(text: &str) -> &'static str {
    let value = text.trim();
    if value.starts_with("https://") || value.starts_with("http://") {
        "url"
    } else if value.starts_with('#')
        && matches!(value.len(), 4 | 7 | 9)
        && value[1..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        "color"
    } else if !value.contains(char::is_whitespace)
        && value.contains('@')
        && value.rsplit('@').next().is_some_and(|v| v.contains('.'))
    {
        "email"
    } else if value.starts_with("\\\\")
        || value
            .as_bytes()
            .get(1..3)
            .is_some_and(|s| s == b":\\" || s == b":/")
    {
        "path"
    } else {
        "plain"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_original_survives_display_redaction() {
        let secret = "api_key=abcdefghijklmno";
        assert!(is_sensitive(secret));
        assert_ne!(display_text(secret, true, true), secret);
        assert_eq!(display_text(secret, true, false), secret);
    }
}
