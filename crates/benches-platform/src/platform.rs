use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

/// What clicking a link opens. A file is shown in the file manager rather than opened, because
/// opening would launch an `.app` or `.command` that program output merely named.
#[derive(Debug, PartialEq)]
pub enum Link {
    Url(String),
    File(PathBuf),
}

pub fn open(link: &Link) -> io::Result<ExitStatus> {
    open_command(link).status()
}

fn open_command(link: &Link) -> Command {
    let mut open = Command::new("/usr/bin/open");
    match link {
        Link::Url(url) => open.arg(url),
        Link::File(path) => open.arg("-R").arg(path),
    };
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(link: Link) -> Vec<String> {
        open_command(&link)
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_url_opens_plainly() {
        assert_eq!(
            args(Link::Url("https://a.test/x".to_owned())),
            ["https://a.test/x"]
        );
    }

    #[test]
    fn a_file_is_revealed_not_opened() {
        assert_eq!(
            args(Link::File("/tmp/a b.app".into())),
            ["-R", "/tmp/a b.app"]
        );
    }
}
