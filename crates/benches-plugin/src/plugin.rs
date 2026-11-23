//! Command palette plugins: the `plugin.toml` manifest and protocol v1, which the host and
//! plugins written in Rust share. See `docs/plugins.md`.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

/// A plugin's `plugin.toml`. Everything the palette shows before Enter comes from here, so
/// typing spawns no process.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Unique among plugins. A user plugin overrides a bundled one with the same name.
    pub name: String,
    pub description: Option<String>,
    /// The executable, relative to the plugin's folder, else found by the login shell.
    pub exec: String,
    /// Commands the login shell must find, or the plugin stays hidden.
    #[serde(default)]
    pub requires: Vec<String>,
    /// How long a call may take, such as `5s` or `2m`, unless a contribution sets its own.
    /// Defaults to 5 seconds.
    pub timeout: Option<String>,
    #[serde(default, rename = "command")]
    pub commands: Vec<CommandContribution>,
    #[serde(default, rename = "prefix")]
    pub prefixes: Vec<PrefixContribution>,
    #[serde(default, rename = "row_action")]
    pub row_actions: Vec<RowActionContribution>,
}

/// A palette command, ranked with the built-in ones. It sets exactly one of `invoke` and `fill`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandContribution {
    pub id: String,
    pub label: String,
    pub when: Option<When>,
    /// The contribution to invoke the plugin with.
    pub invoke: Option<String>,
    /// Text to put in the palette's input instead, without running the plugin.
    pub fill: Option<String>,
    /// Shown while the invoke runs, keeping the palette busy until it answers or Esc cancels it.
    pub busy: Option<String>,
    /// Shown while the rows of a page load, when `busy` is unset. Defaults to `Loading…`.
    pub loading: Option<String>,
    pub timeout: Option<String>,
}

/// A row offered once the query starts with `prefix` and the rest of it matches `arg`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrefixContribution {
    pub id: String,
    pub prefix: String,
    /// A regular expression the query after `prefix` must match. Defaults to `^$`, nothing.
    pub arg: Option<String>,
    /// The row's label, where `{arg}` is the query after `prefix`, trimmed, and
    /// `{project.name}` the active project's name.
    pub label: String,
    /// The label when `{arg}` isn't empty. Defaults to `label`.
    pub label_with_arg: Option<String>,
    pub when: Option<When>,
    pub busy: Option<String>,
    pub loading: Option<String>,
    pub timeout: Option<String>,
}

/// A secondary action on a row the host owns, invoked with that row's subject.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RowActionContribution {
    pub id: String,
    pub subject: SubjectKind,
    #[serde(default)]
    pub chord: RowChord,
    /// Shown after the chord below the selected row.
    pub hint: String,
    pub busy: Option<String>,
    pub loading: Option<String>,
    pub timeout: Option<String>,
}

/// When a contribution is offered.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum When {
    /// A project is open.
    #[serde(rename = "project")]
    Project,
    #[serde(rename = "torn_off")]
    TornOff,
    #[serde(rename = "!torn_off")]
    NotTornOff,
    #[serde(rename = "projects>1")]
    SeveralProjects,
}

/// The rows a row action applies to.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    /// The jump command's folder.
    Folder,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum RowChord {
    #[default]
    #[serde(rename = "mod+enter")]
    ModEnter,
}

/// What the host sends a plugin, one JSON object per line.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Request {
    pub v: u32,
    pub id: u64,
    pub method: Method,
    pub contribution: String,
    pub arg: Option<String>,
    pub subject: Option<Subject>,
    /// A page row's `data`, echoed back when the row invokes the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub context: Context,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    Invoke,
}

/// A row the host owns, passed to a plugin as what it acts on.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Folder { path: PathBuf },
}

/// What the host knows of the window a request comes from.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Context {
    pub project: Option<ProjectInfo>,
    /// The active tab's directory.
    pub cwd: Option<PathBuf>,
    pub working_directory: PathBuf,
    /// `macos` or `linux`.
    pub platform: String,
    pub projects: Vec<ProjectInfo>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub name: String,
}

/// A plugin's answer to the request with the same `id`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub answer: Answer,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    /// Run these, closing the palette unless they fill it or report an error.
    Effects(Vec<Effect>),
    /// Show these as a page the user filters by typing.
    Rows(Vec<Row>),
    /// Shown in the palette's error line.
    Error(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Row {
    pub label: String,
    /// Shown dimmed after the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Words that match typing without being shown.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Echoed back in the request when `enter` invokes the plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub enter: Enter,
}

/// What Enter on a page row does.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Enter {
    Effects(Vec<Effect>),
    Invoke { invoke: String },
}

/// What the host does with an answer. The vocabulary is closed: plugins never touch the UI.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    OpenProject {
        path: PathBuf,
    },
    OpenTab {
        title: String,
        command: Option<String>,
        cwd: Option<PathBuf>,
    },
    OpenTray {
        command: String,
    },
    SwitchProject {
        root: PathBuf,
    },
    /// A built-in command by its id, such as `new_shell`.
    RunCommand {
        id: String,
    },
    /// Puts `text` in the palette's input, keeping it open.
    Fill {
        text: String,
    },
    OpenUrl {
        url: String,
    },
    Error {
        message: String,
    },
    Close,
}

/// The JSON schema of `plugin.toml`.
pub fn schema() -> String {
    format!("{:#}\n", schemars::schema_for!(Manifest).as_value())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worktrunk_manifest_parses() {
        let manifest: Manifest = toml::from_str(
            r#"
name = "worktrunk"
description = "Git worktrees through Worktrunk's `wt`"
exec = "benches-worktrunk"
requires = ["wt"]

[[command]]
id = "open"
label = "Open worktree…"
when = "project"
invoke = "list"
loading = "Loading worktrees…"

[[command]]
id = "create"
label = "Create worktree…"
when = "project"
fill = "wt "

[[prefix]]
id = "create"
prefix = "wt "
arg = '^[^-\s]\S*$'
label = "Create worktree “{arg}”"
when = "project"
busy = "Creating worktree…"
timeout = "120s"

[[prefix]]
id = "list"
prefix = "wts"
arg = '^(| .*)$'
label = "Open a worktree of {project.name}"
label_with_arg = "Open a worktree of {project.name} matching “{arg}”"
when = "project"

[[row_action]]
id = "list"
subject = "folder"
chord = "mod+enter"
hint = "worktrees"
"#,
        )
        .unwrap();
        assert_eq!(manifest.commands.len(), 2);
        assert_eq!(manifest.commands[0].invoke.as_deref(), Some("list"));
        assert_eq!(manifest.prefixes[1].when, Some(When::Project));
        assert_eq!(manifest.row_actions[0].subject, SubjectKind::Folder);
    }

    #[test]
    fn unknown_manifest_keys_are_errors() {
        let error = toml::from_str::<Manifest>("name = \"x\"\nexec = \"x\"\nmode = \"persistent\"")
            .unwrap_err();
        assert!(error.to_string().contains("mode"), "{error}");
    }

    #[test]
    fn requests_and_responses_read_as_the_docs_show() {
        let request = Request {
            v: PROTOCOL_VERSION,
            id: 7,
            method: Method::Invoke,
            contribution: "create".to_owned(),
            arg: Some("feature-x".to_owned()),
            subject: None,
            data: None,
            context: Context::default(),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(
            line.starts_with(r#"{"v":1,"id":7,"method":"invoke","contribution":"create","arg":"feature-x","subject":null,"context":{"#),
            "{line}"
        );
        let subject = serde_json::to_value(Subject::Folder {
            path: PathBuf::from("/dev/site"),
        })
        .unwrap();
        assert_eq!(
            subject,
            serde_json::json!({"folder": {"path": "/dev/site"}})
        );

        let parse = |line: &str| serde_json::from_str::<Response>(line).unwrap();
        assert_eq!(
            parse(
                r#"{"id":7,"effects":[{"open_project":{"path":"/dev/site.feature-x"}},"close"]}"#
            ),
            Response {
                id: 7,
                answer: Answer::Effects(vec![
                    Effect::OpenProject {
                        path: PathBuf::from("/dev/site.feature-x")
                    },
                    Effect::Close
                ]),
            }
        );
        assert_eq!(
            parse(r#"{"id":7,"error":"✗ Branch feature-x already exists"}"#).answer,
            Answer::Error("✗ Branch feature-x already exists".to_owned())
        );
        let rows = parse(
            r#"{"id":9,"rows":[
                {"label":"main  ~/dev/site","data":{"path":"/dev/site"},"enter":[{"open_project":{"path":"/dev/site"}}]},
                {"label":"feature","keywords":["/dev/site.feature"],"enter":{"invoke":"open"}}
            ]}"#,
        );
        let Answer::Rows(rows) = rows.answer else {
            panic!("not rows");
        };
        assert_eq!(rows[0].data, Some(serde_json::json!({"path": "/dev/site"})));
        assert_eq!(
            rows[1].enter,
            Enter::Invoke {
                invoke: "open".to_owned()
            }
        );
    }

    #[test]
    fn the_committed_schema_is_current() {
        let committed = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schema/plugin.schema.json"
        ));
        assert!(
            committed == schema(),
            "schema/plugin.schema.json is out of date; run `cargo run -- plugin schema > schema/plugin.schema.json`"
        );
    }
}
