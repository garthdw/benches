//! Command palette plugins: finding them, checking their manifests and what they require, and
//! running them once per request. See `docs/plugins.md`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use benches_plugin::{Answer, Manifest, PrefixContribution, Request, Response};
use regex_lite::Regex;
use serde_json::Value;

use crate::LogErr;
use crate::config::parse_duration;
use crate::process::{Cancel, login_args, run, run_with_input, strip_escapes};

const MANIFEST: &str = "plugin.toml";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the login shell may take to say whether a required command exists.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A plugin whose manifest checked out.
#[derive(Debug)]
pub struct Plugin {
    pub manifest: Manifest,
    pub dir: PathBuf,
    /// `manifest.prefixes`' `arg` patterns, in order.
    args: Vec<Regex>,
}

impl Plugin {
    /// The plugin in `dir`, unless `disabled` names it, which skips its checks too.
    fn load(dir: &Path, disabled: &[String]) -> Result<Option<Self>> {
        let path = dir.join(MANIFEST);
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let manifest: Manifest =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if disabled.contains(&manifest.name) {
            return Ok(None);
        }
        let args = check(&manifest).with_context(|| format!("check {}", path.display()))?;
        Ok(Some(Self {
            manifest,
            dir: dir.to_owned(),
            args,
        }))
    }

    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The prefix contributions `query` starts with whose rest matches `arg`, each with that rest
    /// trimmed.
    pub fn prefixes<'a>(
        &'a self,
        query: &'a str,
    ) -> impl Iterator<Item = (&'a PrefixContribution, &'a str)> {
        self.manifest
            .prefixes
            .iter()
            .zip(&self.args)
            .filter_map(move |(prefix, arg)| {
                let rest = query.strip_prefix(&prefix.prefix)?;
                arg.is_match(rest).then_some((prefix, rest.trim()))
            })
    }

    /// A contribution's own timeout, else the plugin's, else five seconds. `check` has parsed
    /// both already.
    pub fn timeout(&self, own: Option<&str>) -> Duration {
        own.or(self.manifest.timeout.as_deref())
            .and_then(|timeout| parse_duration(timeout).ok())
            .unwrap_or(DEFAULT_TIMEOUT)
    }

    /// The executable in the plugin's folder, else the name for the login shell to find.
    fn exec(&self) -> String {
        let bundled = self.dir.join(&self.manifest.exec);
        if bundled.is_file() {
            bundled.display().to_string()
        } else {
            self.manifest.exec.clone()
        }
    }
}

/// What the manifest's types can't say, returning the compiled `arg` patterns.
fn check(manifest: &Manifest) -> Result<Vec<Regex>> {
    ensure!(!manifest.name.is_empty(), "`name` is empty");
    ensure!(!manifest.exec.is_empty(), "`exec` is empty");
    let commands = manifest.commands.iter().map(|command| &command.id);
    let prefixes = manifest.prefixes.iter().map(|prefix| &prefix.id);
    let row_actions = manifest.row_actions.iter().map(|action| &action.id);
    for (kind, ids) in [
        ("command", commands.collect::<Vec<_>>()),
        ("prefix", prefixes.collect()),
        ("row_action", row_actions.collect()),
    ] {
        let mut seen = HashSet::new();
        if let Some(id) = ids.into_iter().find(|id| !seen.insert(*id)) {
            bail!("two {kind}s have the id `{id}`");
        }
    }
    for command in &manifest.commands {
        ensure!(
            command.invoke.is_some() != command.fill.is_some(),
            "command `{}` needs exactly one of `invoke` and `fill`",
            command.id
        );
    }
    let timeouts = [&manifest.timeout]
        .into_iter()
        .chain(manifest.commands.iter().map(|command| &command.timeout))
        .chain(manifest.prefixes.iter().map(|prefix| &prefix.timeout))
        .chain(manifest.row_actions.iter().map(|action| &action.timeout));
    for timeout in timeouts.flatten() {
        parse_duration(timeout).with_context(|| format!("`timeout = {timeout:?}`"))?;
    }
    manifest
        .prefixes
        .iter()
        .map(|prefix| {
            let arg = prefix.arg.as_deref().unwrap_or("^$");
            Regex::new(arg).with_context(|| format!("prefix `{}`'s `arg`", prefix.id))
        })
        .collect()
}

/// The plugins in each of `dirs`' subfolders, where one in a later folder replaces an earlier
/// one with the same name, leaving out those `disabled` names. A plugin that fails its checks
/// is left out and reported.
pub fn discover(dirs: &[PathBuf], disabled: &[String]) -> (Vec<Plugin>, Vec<anyhow::Error>) {
    let mut plugins: Vec<Plugin> = Vec::new();
    let mut errors = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut folders: Vec<PathBuf> = entries
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.join(MANIFEST).is_file())
            .collect();
        folders.sort();
        for folder in folders {
            match Plugin::load(&folder, disabled) {
                Ok(None) => {}
                Ok(Some(plugin)) => match plugins
                    .iter_mut()
                    .find(|known| known.name() == plugin.name())
                {
                    Some(known) => *known = plugin,
                    None => plugins.push(plugin),
                },
                Err(error) => errors.push(error),
            }
        }
    }
    (plugins, errors)
}

/// The plugins whose required commands the login shell finds, asking once per command.
pub fn available(shell: &Path, plugins: Vec<Plugin>) -> Vec<Plugin> {
    let required: HashSet<String> = plugins
        .iter()
        .flat_map(|plugin| plugin.manifest.requires.iter().cloned())
        .collect();
    // Each check starts a login shell, so they run side by side.
    let missing: HashSet<&String> = std::thread::scope(|scope| {
        let checks: Vec<_> = required
            .iter()
            .map(|command| (command, scope.spawn(move || finds(shell, command))))
            .collect();
        checks
            .into_iter()
            .filter_map(|(command, check)| (!matches!(check.join(), Ok(true))).then_some(command))
            .collect()
    });
    plugins
        .into_iter()
        .filter(|plugin| {
            let requires = &plugin.manifest.requires;
            requires.iter().all(|command| !missing.contains(command))
        })
        .collect()
}

fn finds(shell: &Path, command: &str) -> bool {
    let args = login_args(shell, "command -v", "", [command].into_iter());
    run(
        shell,
        &args,
        Path::new("/"),
        PROBE_TIMEOUT,
        &format!("command -v {command}"),
        &Cancel::default(),
    )
    .log_err("plugin requirement")
    .is_some_and(|output| output.status.success())
}

/// A request id unique to this app run, so an answer can't be taken for another's.
pub fn next_request_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Runs the plugin once through the login shell in its folder, with `request` on its stdin, and
/// returns its answer to it. Lines before the answer, such as a shell's banner, are skipped.
pub fn invoke(
    shell: &Path,
    plugin: &Plugin,
    request: &Request,
    timeout: Duration,
    cancel: &Cancel,
) -> Result<Answer> {
    let exec = plugin.exec();
    // The path goes in as an argument, so nothing in it is interpreted by the shell.
    let args = login_args(shell, "exec", "", [exec.as_str()].into_iter());
    let mut input = serde_json::to_vec(request).context("encode request")?;
    input.push(b'\n');
    let output = run_with_input(
        shell,
        &args,
        &plugin.dir,
        &input,
        timeout,
        plugin.name(),
        cancel,
    )?;
    // A login shell's rc may print escapes or a greeting ahead of the answer, even on its line.
    let stdout = strip_escapes(&String::from_utf8_lossy(&output.stdout));
    let answer = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(&line[line.find('{')?..]).ok())
        .find(|value| value.get("id").and_then(Value::as_u64) == Some(request.id));
    if let Some(answer) = answer {
        let response: Response = serde_json::from_value(answer)
            .with_context(|| format!("{} answered in a form Benches can't read", plugin.name()))?;
        return Ok(response.answer);
    }
    let stderr = strip_escapes(&String::from_utf8_lossy(&output.stderr));
    match stderr.lines().map(str::trim).rfind(|line| !line.is_empty()) {
        Some(reason) => bail!("{}: {reason}", plugin.name()),
        None if !output.status.success() => bail!("{} failed: {}", plugin.name(), output.status),
        None => bail!("{} gave no answer", plugin.name()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::stub_plugin;
    use benches_plugin::{Context, Effect, Method};

    const SHELL: &str = "/bin/sh";

    fn manifest(name: &str, rest: &str) -> String {
        format!("name = \"{name}\"\nexec = \"run\"\n{rest}")
    }

    fn names(plugins: &[Plugin]) -> Vec<&str> {
        plugins.iter().map(Plugin::name).collect()
    }

    /// `plugins` as folders of one plugins folder.
    fn plugins_dir(plugins: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (folder, manifest) in plugins {
            let folder = dir.path().join(folder);
            std::fs::create_dir(&folder).unwrap();
            std::fs::write(folder.join(MANIFEST), manifest).unwrap();
        }
        dir
    }

    #[test]
    fn a_user_plugin_replaces_a_bundled_one_and_disabled_ones_are_left_out() {
        let bundled = plugins_dir(&[
            ("a", &manifest("worktrunk", "description = \"bundled\"")),
            ("b", &manifest("other", "")),
        ]);
        let user = plugins_dir(&[("mine", &manifest("worktrunk", "description = \"user\""))]);
        let dirs = [bundled.path().to_owned(), user.path().to_owned()];
        let (plugins, errors) = discover(&dirs, &[]);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(names(&plugins), ["worktrunk", "other"]);
        assert_eq!(plugins[0].manifest.description.as_deref(), Some("user"));
        let (plugins, _) = discover(&dirs, &["worktrunk".to_owned()]);
        assert_eq!(names(&plugins), ["other"]);
        let (plugins, errors) = discover(&[PathBuf::from("/no/such/folder")], &[]);
        assert!(plugins.is_empty() && errors.is_empty());
    }

    #[test]
    fn a_broken_manifest_is_reported_and_the_rest_still_load() {
        let dir = plugins_dir(&[
            ("bad-toml", "name = "),
            ("unknown-key", &manifest("u", "mode = \"persistent\"")),
            (
                "both",
                &manifest(
                    "b",
                    "[[command]]\nid = \"x\"\nlabel = \"X\"\ninvoke = \"x\"\nfill = \"x\"",
                ),
            ),
            (
                "neither",
                &manifest("n", "[[command]]\nid = \"x\"\nlabel = \"X\""),
            ),
            (
                "regex",
                &manifest(
                    "r",
                    "[[prefix]]\nid = \"x\"\nprefix = \"x\"\narg = \"(\"\nlabel = \"X\"",
                ),
            ),
            (
                "twice",
                &manifest(
                    "t",
                    "[[prefix]]\nid = \"x\"\nprefix = \"x\"\nlabel = \"X\"\n[[prefix]]\nid = \"x\"\nprefix = \"y\"\nlabel = \"Y\"",
                ),
            ),
            ("timeout", &manifest("s", "timeout = \"soon\"")),
            ("good", &manifest("good", "")),
        ]);
        let (plugins, errors) = discover(&[dir.path().to_owned()], &[]);
        assert_eq!(names(&plugins), ["good"]);
        assert_eq!(errors.len(), 7, "{errors:?}");
        let both = format!("{:#}", errors[1]);
        assert!(
            both.contains("both/plugin.toml") && both.contains("exactly one"),
            "{both}"
        );
        let (_, errors) = discover(&[dir.path().to_owned()], &["r".to_owned()]);
        assert_eq!(
            errors.len(),
            6,
            "a disabled plugin goes unchecked: {errors:?}"
        );
    }

    #[test]
    fn prefixes_offer_their_trimmed_argument_when_it_matches() {
        let dir = plugins_dir(&[(
            "wt",
            &manifest(
                "worktrunk",
                r#"
[[prefix]]
id = "create"
prefix = "wt "
arg = '^[^-\s]\S*$'
label = "Create worktree “{arg}”"

[[prefix]]
id = "list"
prefix = "wts"
arg = '^(| .*)$'
label = "Open a worktree"
"#,
            ),
        )]);
        let (plugins, _) = discover(&[dir.path().to_owned()], &[]);
        let offered = |query| -> Vec<(String, String)> {
            plugins[0]
                .prefixes(query)
                .map(|(prefix, arg)| (prefix.id.clone(), arg.to_owned()))
                .collect()
        };
        let one = |id: &str, arg: &str| vec![(id.to_owned(), arg.to_owned())];
        assert_eq!(offered("wt feature-x"), one("create", "feature-x"));
        assert_eq!(offered("wts"), one("list", ""));
        assert_eq!(offered("wts fea"), one("list", "fea"));
        for query in ["wt", "wt -x", "wt two words", "wtsx", "wtf x"] {
            assert_eq!(offered(query), [], "{query}");
        }
    }

    #[test]
    fn a_plugin_needing_a_missing_command_is_hidden() {
        let dir = plugins_dir(&[
            ("has", &manifest("has", "requires = [\"sh\"]")),
            (
                "lacks",
                &manifest("lacks", "requires = [\"sh\", \"benches-no-such-command\"]"),
            ),
        ]);
        let (plugins, _) = discover(&[dir.path().to_owned()], &[]);
        assert_eq!(names(&available(Path::new(SHELL), plugins)), ["has"]);
    }

    fn request(contribution: &str) -> Request {
        Request {
            v: benches_plugin::PROTOCOL_VERSION,
            id: next_request_id(),
            method: Method::Invoke,
            contribution: contribution.to_owned(),
            arg: Some("feature x".to_owned()),
            subject: None,
            data: None,
            context: Context::default(),
        }
    }

    fn load(script: &str) -> (tempfile::TempDir, Plugin) {
        let dir = stub_plugin(&manifest("stub", ""), script);
        let plugin = Plugin::load(&dir.path().join("stub"), &[])
            .unwrap()
            .unwrap();
        (dir, plugin)
    }

    fn call(plugin: &Plugin, request: &Request) -> Result<Answer> {
        invoke(
            Path::new(SHELL),
            plugin,
            request,
            Duration::from_secs(15),
            &Cancel::default(),
        )
    }

    #[test]
    fn invoke_sends_the_request_on_stdin_and_takes_the_answer_with_its_id() {
        let (dir, plugin) = load(
            r#"read -r line
printf '%s\n' "$line" > request.json
id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
echo 'a banner'
echo '{"id":0,"error":"not mine"}'
printf '{"id":%s,"effects":[{"fill":{"text":"wt "}}]}\n' "$id""#,
        );
        let request = request("create");
        assert_eq!(
            call(&plugin, &request).unwrap(),
            Answer::Effects(vec![Effect::Fill {
                text: "wt ".to_owned()
            }])
        );
        let sent = std::fs::read_to_string(dir.path().join("stub/request.json")).unwrap();
        let sent: Request = serde_json::from_str(&sent).unwrap();
        assert_eq!(sent, request);
    }

    /// A script answering the request's id with `answer`'s JSON fields, after `before`.
    fn answering(before: &str, answer: &str) -> String {
        format!(
            r#"read -r line
id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
printf '{before}{{"id":%s,{answer}}}\n' "$id""#
        )
    }

    #[test]
    fn an_answer_after_shell_noise_on_its_line_is_still_read() {
        let (_dir, plugin) = load(&answering(r"\033[?1h\033=hello ", r#""effects":["close"]"#));
        let answer = call(&plugin, &request("list")).unwrap();
        assert_eq!(answer, Answer::Effects(vec![Effect::Close]));
    }

    #[test]
    fn a_malformed_answer_says_what_is_wrong_with_it() {
        let (_dir, plugin) = load(&answering("", r#""effects":[{"open_split":{}}]"#));
        let error = format!("{:#}", call(&plugin, &request("list")).unwrap_err());
        assert!(error.contains("open_split"), "{error}");
    }

    #[test]
    fn a_plugin_that_fails_without_answering_reports_its_last_error_line() {
        let (_dir, plugin) =
            load("echo 'first' >&2; printf '\\033[31m✗ no repository\\033[0m\\n' >&2; exit 3");
        let error = call(&plugin, &request("list")).unwrap_err();
        assert_eq!(format!("{error:#}"), "stub: ✗ no repository");
        let (_dir, plugin) = load("exit 0");
        let error = call(&plugin, &request("list")).unwrap_err();
        assert_eq!(format!("{error:#}"), "stub gave no answer");
    }

    #[test]
    fn a_plugin_outlasting_its_timeout_is_ended() {
        let (_dir, plugin) = load("sleep 30");
        let error = invoke(
            Path::new(SHELL),
            &plugin,
            &request("list"),
            Duration::from_millis(300),
            &Cancel::default(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("took longer"), "{error:#}");
    }

    #[test]
    fn timeouts_fall_back_from_the_contribution_to_the_plugin_to_five_seconds() {
        let dir = plugins_dir(&[
            ("a", &manifest("a", "timeout = \"2m\"")),
            ("b", &manifest("b", "")),
        ]);
        let (plugins, _) = discover(&[dir.path().to_owned()], &[]);
        assert_eq!(plugins[0].timeout(Some("120s")), Duration::from_secs(120));
        assert_eq!(plugins[0].timeout(None), Duration::from_secs(120));
        assert_eq!(plugins[1].timeout(None), DEFAULT_TIMEOUT);
    }
}
