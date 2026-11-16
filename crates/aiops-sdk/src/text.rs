//! Bounding strings before they leave the host (D-014): the SDK cuts to the caps and
//! says so (`truncated`), so the collector never has to refuse for length.

/// `text` clipped to at most `max` bytes on a character boundary; `true` if it was cut.
pub(crate) fn clip(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_owned(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// An identifier: `[A-Za-z0-9._:/@-]`, other characters replaced with `_`, non-empty,
/// at most `max` bytes. `true` if anything was changed.
pub(crate) fn ident(text: &str, max: usize) -> (String, bool) {
    let mut changed = false;
    let mut out: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@') {
                c
            } else {
                changed = true;
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
        changed = true;
    }
    if out.len() > max {
        out.truncate(max);
        changed = true;
    }
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clips_on_char_boundaries_and_sanitizes_identifiers() {
        assert_eq!(clip("héllo", 2), ("h".to_owned(), true));
        assert_eq!(clip("ok", 2), ("ok".to_owned(), false));
        assert_eq!(
            ident("publish article!", 128),
            ("publish_article_".to_owned(), true)
        );
        assert_eq!(ident("", 8), ("_".to_owned(), true));
        assert_eq!(ident("a".repeat(200).as_str(), 128).0.len(), 128);
    }
}
