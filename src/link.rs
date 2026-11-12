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

/// Arguments for `/usr/bin/open`. A file is revealed in Finder rather than opened, because
/// opening would launch an `.app` or `.command` that program output merely named.
pub fn open_args(uri: &str) -> Vec<String> {
    file_path(uri).map_or_else(|| vec![uri.to_owned()], |path| vec!["-R".to_owned(), path])
}

/// The host and path of a `file://` URI.
fn file_parts(uri: &str) -> Option<(&str, String)> {
    let rest = uri
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file://"))
        .and_then(|_| uri.get(7..))?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    Some((host, percent_decode(path)))
}

/// The path of a `file://` URI, without its host such as `localhost`.
pub fn file_path(uri: &str) -> Option<String> {
    file_parts(uri).map(|(_, path)| path)
}

/// The local directory a program reported with OSC 7 (a `file://` URI) or OSC 9;9 and 1337 (a
/// bare path). A URI naming another host, such as one sent from inside ssh, isn't local, and a bare
/// path has no host to check, so it must exist here.
pub fn reported_directory(reported: &[u8], hostname: &str) -> Option<std::path::PathBuf> {
    let reported = std::str::from_utf8(reported).ok()?;
    let path = match file_parts(reported) {
        Some((host, path)) => is_local_host(host, hostname).then_some(path)?,
        None => reported.to_owned(),
    };
    let path = std::path::PathBuf::from(path);
    let local = path.is_absolute() && (file_parts(reported).is_some() || path.is_dir());
    local.then_some(path)
}

fn is_local_host(host: &str, hostname: &str) -> bool {
    let short = hostname.split('.').next().unwrap_or(hostname);
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case(hostname)
        || host.eq_ignore_ascii_case(short)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let escaped = (byte == b'%')
            .then(|| text.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        decoded.push(escaped.unwrap_or(byte));
        at += if escaped.is_some() { 3 } else { 1 };
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The URL under `column` in a logical line of cells, one `char` per column.
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

    #[test]
    fn a_reported_directory_is_decoded_and_local() {
        let directory = |reported: &str| reported_directory(reported.as_bytes(), "Mac.local");
        let path = |path: &str| Some(std::path::PathBuf::from(path));
        assert_eq!(
            directory("file://mac.LOCAL/Users/me/My%20Dir"),
            path("/Users/me/My Dir")
        );
        assert_eq!(directory("file://MAC/tmp"), path("/tmp"));
        assert_eq!(directory("file:///tmp/caf%C3%A9"), path("/tmp/café"));
        assert_eq!(directory("file://localhost/"), path("/"));
        assert_eq!(directory("file://other.example/tmp"), None);
        assert_eq!(directory("file://mac.example/tmp"), None);
        assert_eq!(directory("file://host"), None);
        assert_eq!(directory("https://example.com/x"), None);
        assert_eq!(directory(""), None);
        assert_eq!(reported_directory(b"file:///\xff", "mac"), None);
    }

    #[test]
    fn a_bare_reported_path_must_be_a_local_directory() {
        let directory = |reported: &str| reported_directory(reported.as_bytes(), "mac");
        assert_eq!(directory("/tmp"), Some("/tmp".into()));
        assert_eq!(directory("/no/such/folder"), None);
        assert_eq!(directory("relative"), None);
    }

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
    fn web_and_mail_links_open_plainly() {
        assert_eq!(open_args("https://a.test/x"), ["https://a.test/x"]);
        assert_eq!(open_args("mailto:a@b.test"), ["mailto:a@b.test"]);
    }

    #[test]
    fn file_links_are_revealed_not_opened() {
        assert_eq!(open_args("file:///tmp/a%20b.app"), ["-R", "/tmp/a b.app"]);
        assert_eq!(open_args("FILE://localhost/tmp/x"), ["-R", "/tmp/x"]);
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
