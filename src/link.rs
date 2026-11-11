const SCHEMES: [&str; 4] = ["https://", "http://", "file://", "mailto:"];

/// Only schemes whose handler is a browser, Finder or a mail client. Anything else, such as
/// `ssh:` or `x-apple.systempreferences:`, would let program output start arbitrary apps.
pub fn openable(uri: &str) -> bool {
    let lowercase = uri.to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| {
        lowercase
            .strip_prefix(scheme)
            .is_some_and(|rest| !rest.is_empty())
    })
}

/// The URL under `column` in one row of cells, one `char` per column. Links that wrap onto
/// the next row are not joined.
pub fn url_at(row: &[char], column: usize) -> Option<String> {
    let delimiter = |character: &char| character.is_whitespace() || "<>\"'`".contains(*character);
    if row.get(column).is_none_or(delimiter) {
        return None;
    }
    let start = row[..column]
        .iter()
        .rposition(delimiter)
        .map_or(0, |at| at + 1);
    let end = row[column..]
        .iter()
        .position(delimiter)
        .map_or(row.len(), |at| column + at);
    let token = &row[start..end];
    let offset = (0..token.len()).find(|&at| {
        (at == 0 || !token[at - 1].is_alphanumeric()) && {
            let rest: String = token[at..].iter().collect::<String>().to_ascii_lowercase();
            SCHEMES.iter().any(|scheme| rest.starts_with(scheme))
        }
    })?;
    let mut url = &token[offset..];
    while let Some(&last) = url.last() {
        let closer = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            _ => None,
        };
        let unbalanced = closer.is_some_and(|opener| {
            url.iter().filter(|&&c| c == last).count()
                > url.iter().filter(|&&c| c == opener).count()
        });
        if !(unbalanced || ".,;:!?".contains(last)) {
            break;
        }
        url = &url[..url.len() - 1];
    }
    let span = start + offset..start + offset + url.len();
    let url: String = url.iter().collect();
    (span.contains(&column) && openable(&url)).then_some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, needle: &str) -> Option<String> {
        let row: Vec<char> = text.chars().collect();
        let column = text.find(needle)?;
        url_at(&row, text[..column].chars().count())
    }

    #[test]
    fn finds_a_url_from_any_column_within_it() {
        let text = "see https://example.com/a?b=1 now";
        for needle in ["https", "example", "b=1"] {
            assert_eq!(
                at(text, needle).as_deref(),
                Some("https://example.com/a?b=1")
            );
        }
        assert_eq!(at(text, "now"), None);
        assert_eq!(at(text, "see"), None);
    }

    #[test]
    fn trims_trailing_punctuation() {
        assert_eq!(
            at("go to https://example.com/x.", "example").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(
            at("(see https://example.com/x), ok", "example").as_deref(),
            Some("https://example.com/x")
        );
    }

    #[test]
    fn keeps_balanced_parentheses() {
        assert_eq!(
            at("https://en.wikipedia.org/wiki/Rust_(language)", "wiki").as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
    }

    #[test]
    fn stops_at_quotes_and_angle_brackets() {
        assert_eq!(
            at("<a href=\"http://a.test/p\">", "a.test").as_deref(),
            Some("http://a.test/p")
        );
    }

    #[test]
    fn finds_file_and_mailto() {
        assert_eq!(
            at("open file:///tmp/a.txt please", "tmp").as_deref(),
            Some("file:///tmp/a.txt")
        );
        assert_eq!(
            at("mail mailto:me@example.com.", "me@").as_deref(),
            Some("mailto:me@example.com")
        );
    }

    #[test]
    fn ignores_other_schemes_and_bare_schemes() {
        assert_eq!(at("ssh://host/path", "host"), None);
        assert_eq!(at("see https:// here", "https"), None);
        assert_eq!(at("nothttps://x.test", "x.test"), None);
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(
            at("HTTPS://X.TEST/a", "X.TEST").as_deref(),
            Some("HTTPS://X.TEST/a")
        );
    }

    #[test]
    fn allow_list() {
        assert!(openable("https://a.test"));
        assert!(openable("HTTP://a.test"));
        assert!(openable("file:///tmp/a"));
        assert!(openable("mailto:a@b.test"));
        assert!(!openable("ssh://host"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("x-apple.systempreferences:"));
        assert!(!openable("https://"));
        assert!(!openable("-a Calculator"));
    }
}
