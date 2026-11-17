use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Format {
    Fish,
    Zsh,
    Bash,
}

/// Enough for suggestions, while bounding what each keystroke searches.
const LIMIT: usize = 10_000;
/// zsh writes a byte it reserves as this, then the byte xor 0x20.
const ZSH_META: u8 = 0x83;

/// Which history file was read and when it was last written, so an unchanged one isn't reread.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stamp {
    path: Option<PathBuf>,
    modified: Option<SystemTime>,
}

/// A history file's stamp and its commands, newest first.
pub type Loaded = (Stamp, Vec<String>);

/// The login shell's history, newest command first with repeats dropped, unless `known` is
/// still current. A missing file is an empty history.
pub fn load(shell: &Path, known: &Stamp) -> anyhow::Result<Option<Loaded>> {
    load_file(history_file(shell, |name| std::env::var_os(name)), known)
}

fn load_file(file: Option<(Format, PathBuf)>, known: &Stamp) -> anyhow::Result<Option<Loaded>> {
    let modified = match &file {
        Some((_, path)) => match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata.modified()?),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
        },
        None => None,
    };
    let stamp = Stamp {
        path: file.as_ref().map(|(_, path)| path.clone()),
        modified,
    };
    if stamp == *known {
        return Ok(None);
    }
    let commands = match file {
        Some((format, path)) if modified.is_some() => {
            let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            recent_first(&parse(format, &decode(format, &bytes)))
        }
        _ => Vec::new(),
    };
    Ok(Some((stamp, commands)))
}

fn history_file(shell: &Path, env: impl Fn(&str) -> Option<OsString>) -> Option<(Format, PathBuf)> {
    let set = |name| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let home = set("HOME")?;
    let (format, default) = match shell.file_name()?.to_str()? {
        "fish" => {
            let data = set("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"));
            return Some((Format::Fish, data.join("fish/fish_history")));
        }
        "zsh" => (Format::Zsh, ".zsh_history"),
        "bash" => (Format::Bash, ".bash_history"),
        _ => return None,
    };
    Some((
        format,
        set("HISTFILE").unwrap_or_else(|| home.join(default)),
    ))
}

/// The file as text, with zsh's reserved bytes restored.
fn decode(format: Format, bytes: &[u8]) -> Cow<'_, str> {
    if format != Format::Zsh || !bytes.contains(&ZSH_META) {
        return String::from_utf8_lossy(bytes);
    }
    let mut unmetafied = Vec::with_capacity(bytes.len());
    let mut iter = bytes.iter();
    while let Some(&byte) = iter.next() {
        match byte {
            ZSH_META => unmetafied.extend(iter.next().map(|next| next ^ 0x20)),
            _ => unmetafied.push(byte),
        }
    }
    Cow::Owned(String::from_utf8_lossy(&unmetafied).into_owned())
}

/// Commands oldest first, as the file lists them.
fn parse(format: Format, text: &str) -> Vec<Cow<'_, str>> {
    let mut commands: Vec<Cow<str>> = Vec::new();
    match format {
        Format::Fish => commands.extend(
            text.lines()
                .filter_map(|line| line.strip_prefix("- cmd: "))
                .map(unescape_fish),
        ),
        Format::Zsh => {
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                let mut command = Cow::Borrowed(strip_zsh_extended(line));
                while let Some(continued) = command.strip_suffix('\\') {
                    command =
                        Cow::Owned(format!("{continued}\n{}", lines.next().unwrap_or_default()));
                }
                commands.push(command);
            }
        }
        Format::Bash => {
            // With `HISTTIMEFORMAT` set, everything up to the next timestamp is one command.
            let mut stamped = false;
            for line in text.lines() {
                if is_bash_timestamp(line) {
                    stamped = true;
                    commands.push(Cow::Borrowed(""));
                    continue;
                }
                match commands.last_mut().filter(|_| stamped) {
                    Some(command) if command.is_empty() => *command = Cow::Borrowed(line),
                    Some(command) => *command = Cow::Owned(format!("{command}\n{line}")),
                    None => commands.push(Cow::Borrowed(line)),
                }
            }
        }
    }
    commands
}

/// Fish escapes only backslashes and newlines in a command.
fn unescape_fish(escaped: &str) -> Cow<'_, str> {
    if !escaped.contains('\\') {
        return Cow::Borrowed(escaped);
    }
    let mut command = String::with_capacity(escaped.len());
    let mut chars = escaped.chars();
    while let Some(char) = chars.next() {
        if char != '\\' {
            command.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => command.push('\n'),
            Some('\\') => command.push('\\'),
            Some(other) => command.extend(['\\', other]),
            None => command.push('\\'),
        }
    }
    Cow::Owned(command)
}

/// `: <start>:<elapsed>;<command>`, written with `EXTENDED_HISTORY`.
fn strip_zsh_extended(line: &str) -> &str {
    line.strip_prefix(": ")
        .and_then(|rest| rest.split_once(';'))
        .filter(|(stamp, _)| {
            stamp
                .chars()
                .all(|char| char.is_ascii_digit() || char == ':')
        })
        .map_or(line, |(_, command)| command)
}

fn is_bash_timestamp(line: &str) -> bool {
    line.strip_prefix('#')
        .is_some_and(|stamp| !stamp.is_empty() && stamp.chars().all(|char| char.is_ascii_digit()))
}

/// `command` shown on one line, each newline marked.
pub fn flattened(command: &str) -> Cow<'_, str> {
    match command.contains('\n') {
        true => Cow::Owned(command.replace('\n', " ⏎ ")),
        false => Cow::Borrowed(command),
    }
}

/// `command` as one line that runs the same, for the command centre's single-line input: a
/// newline ends a command unless the line already leads into the next. None when a newline is
/// inside quotes or a here-document, which no join keeps.
pub fn single_line(command: &str) -> Option<Cow<'_, str>> {
    if !command.contains('\n') {
        return Some(Cow::Borrowed(command));
    }
    if command.contains("<<") {
        return None;
    }
    let mut line = String::with_capacity(command.len());
    let mut quote = None;
    let mut escaped = false;
    // Held until more follows, so blank and trailing lines add nothing.
    let mut separator = None;
    for char in command.chars() {
        if escaped {
            escaped = false;
            if char == '\n' {
                line.pop();
                line.push(' ');
                continue;
            }
        } else {
            match (quote, char) {
                (Some('\''), '\'') | (Some('"'), '"') => quote = None,
                (Some(_), '\n') => return None,
                (None, '\'' | '"') => quote = Some(char),
                (None | Some('"'), '\\') => escaped = true,
                // A comment would swallow whatever is joined after it.
                (None, '#')
                    if separator.is_some()
                        || line.is_empty()
                        || line.ends_with(char::is_whitespace) =>
                {
                    return None;
                }
                (None, '\n') => {
                    line.truncate(line.trim_end().len());
                    separator = Some(if leads_on(&line) { " " } else { "; " });
                    continue;
                }
                _ => {}
            }
        }
        if let Some(separator) = separator.take().filter(|_| !line.is_empty()) {
            line.push_str(separator);
        }
        line.push(char);
    }
    Some(Cow::Owned(line))
}

/// Whether a line ending like this carries on to the next with no `;` between them.
fn leads_on(line: &str) -> bool {
    let words = ["do", "then", "else"];
    line.ends_with(['{', '(', '|', '&', ';'])
        || words.iter().any(|word| {
            line.strip_suffix(word)
                .is_some_and(|before| before.is_empty() || before.ends_with([' ', '\t', ';']))
        })
}

fn recent_first(commands: &[Cow<'_, str>]) -> Vec<String> {
    let mut seen = HashSet::new();
    commands
        .iter()
        .rev()
        .map(|command| command.as_ref())
        .filter(|command| !command.is_empty() && seen.insert(*command))
        .take(LIMIT)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(format: Format, bytes: &[u8]) -> Vec<String> {
        parse(format, &decode(format, bytes))
            .into_iter()
            .map(Cow::into_owned)
            .collect()
    }

    #[test]
    fn fish_entries_unescape_backslashes_and_newlines() {
        let file = "- cmd: ls\n  when: 1700000000\n- cmd: echo a\\\\nb\n  when: 1\n  paths:\n    - a\\nb\n- cmd: printf 'x\\ny'\n  when: 2\n";
        assert_eq!(
            parsed(Format::Fish, file.as_bytes()),
            ["ls", "echo a\\nb", "printf 'x\ny'"]
        );
    }

    #[test]
    fn zsh_reads_plain_and_extended_lines_and_continuations() {
        let file = b": 1700000000:0;git status\nls -la\n: 1700000001:3;for x in a b; do\\\necho $x\\\ndone\n: 1700000002:0;a;b\n\n";
        assert_eq!(
            parsed(Format::Zsh, file),
            [
                "git status",
                "ls -la",
                "for x in a b; do\necho $x\ndone",
                "a;b",
                ""
            ]
        );
    }

    #[test]
    fn zsh_unmetafies_bytes() {
        // zsh stores a byte it reserves as 0x83 then the byte xor 0x20, here É's 0x89.
        let file = b": 1:0;echo caf\xc3\x83\xa9\n";
        assert_eq!(parsed(Format::Zsh, file), ["echo cafÉ"]);
    }

    #[test]
    fn zsh_survives_invalid_utf8() {
        let file = b"echo \xff\nls\n";
        assert_eq!(parsed(Format::Zsh, file)[1], "ls");
    }

    #[test]
    fn bash_reads_a_command_per_line_without_timestamps() {
        let file = b"make test\n# a comment\n\nls\n";
        assert_eq!(
            parsed(Format::Bash, file),
            ["make test", "# a comment", "", "ls"]
        );
    }

    #[test]
    fn bash_joins_the_lines_between_timestamps() {
        let file = b"old\n#1700000000\nfor x in a; do\necho $x\ndone\n#1700000001\nls\n";
        assert_eq!(
            parsed(Format::Bash, file),
            ["old", "for x in a; do\necho $x\ndone", "ls"]
        );
    }

    #[test]
    fn newest_first_without_repeats_or_blanks() {
        assert_eq!(
            recent_first(&["ls", "make", "", "a\nb", "ls", "git"].map(Cow::Borrowed)),
            ["git", "ls", "a\nb", "make"]
        );
    }

    #[test]
    fn newlines_are_marked_when_shown_on_one_line() {
        assert_eq!(flattened("ls"), "ls");
        assert_eq!(flattened("a\nb"), "a ⏎ b");
    }

    #[test]
    fn multi_line_commands_join_into_one_that_runs_the_same() {
        for (command, line) in [
            ("ls -la", "ls -la"),
            (
                "for x in a b; do\necho $x\ndone",
                "for x in a b; do echo $x; done",
            ),
            (
                "if true\nthen\necho y\nelse\necho n\nfi",
                "if true; then echo y; else echo n; fi",
            ),
            ("make \\\n  test", "make    test"),
            ("ls |\ngrep x", "ls | grep x"),
            ("a &&\nb ||\nc", "a && b || c"),
            ("f() {\necho\n}", "f() { echo; }"),
            ("(cd x\nls)", "(cd x; ls)"),
            ("sleep 1 &\nwait", "sleep 1 & wait"),
            ("a;\n\nb\n", "a; b"),
            ("undo\nls", "undo; ls"),
            ("echo \"it's\"\nls", "echo \"it's\"; ls"),
            ("echo a#b\nls", "echo a#b; ls"),
        ] {
            assert_eq!(single_line(command).as_deref(), Some(line), "{command:?}");
        }
        for quoted in [
            "echo 'a\nb'",
            "echo \"a\nb\"",
            "cat <<EOF\nx\nEOF",
            "echo hi # note\nls",
            "# note\nls",
            "ls\n# note\npwd",
        ] {
            assert_eq!(single_line(quoted), None, "{quoted:?}");
        }
    }

    #[test]
    fn the_history_file_follows_the_shell_and_its_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let home: &[_] = &[("HOME", "/home/me")];
        assert_eq!(
            history_file(Path::new("/opt/homebrew/bin/fish"), env(home)),
            Some((
                Format::Fish,
                PathBuf::from("/home/me/.local/share/fish/fish_history")
            ))
        );
        assert_eq!(
            history_file(
                Path::new("/bin/fish"),
                env(&[("HOME", "/home/me"), ("XDG_DATA_HOME", "/data")])
            ),
            Some((Format::Fish, PathBuf::from("/data/fish/fish_history")))
        );
        assert_eq!(
            history_file(Path::new("/bin/zsh"), env(home)),
            Some((Format::Zsh, PathBuf::from("/home/me/.zsh_history")))
        );
        assert_eq!(
            history_file(
                Path::new("/bin/zsh"),
                env(&[("HOME", "/home/me"), ("HISTFILE", "/h/zsh")])
            ),
            Some((Format::Zsh, PathBuf::from("/h/zsh")))
        );
        assert_eq!(
            history_file(Path::new("/bin/bash"), env(home)),
            Some((Format::Bash, PathBuf::from("/home/me/.bash_history")))
        );
        assert_eq!(history_file(Path::new("/bin/sh"), env(home)), None);
        assert_eq!(history_file(Path::new("/bin/zsh"), env(&[])), None);
    }

    #[test]
    fn a_missing_file_is_an_empty_history_and_an_unchanged_one_is_not_reread() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history");
        let file = || Some((Format::Zsh, path.clone()));
        let (missing, commands) = load_file(file(), &Stamp::default()).unwrap().unwrap();
        assert!(commands.is_empty());
        assert!(load_file(file(), &missing).unwrap().is_none());
        std::fs::write(&path, ": 1:0;ls\n: 2:0;a\\\nb\n: 3:0;make\n: 4:0;ls\n").unwrap();
        let (written, commands) = load_file(file(), &missing).unwrap().unwrap();
        assert_eq!(commands, ["ls", "make", "a\nb"]);
        assert!(load_file(file(), &written).unwrap().is_none());
        assert!(load_file(None, &Stamp::default()).unwrap().is_none());
    }

    #[test]
    fn an_unreadable_file_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let file = Some((Format::Zsh, directory.path().to_owned()));
        assert!(load_file(file, &Stamp::default()).is_err());
    }
}
