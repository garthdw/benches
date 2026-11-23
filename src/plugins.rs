//! The command centre's side of palette plugins: loading them for every window, the rows their
//! manifests offer, calling them, and running the effects they answer with.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use benches_core::LogErr;
use benches_core::plugins::{self, Plugin};
use benches_core::process::Cancel;
use benches_platform::Link;
use benches_plugin::{
    Answer, Context, Effect, Enter, Method, PROTOCOL_VERSION, ProjectInfo, Request, Row, Subject,
    SubjectKind, When,
};
use rune::Tx;
use serde_json::Value;

use crate::app::{State, open_project, open_tab, open_tray, run_command};
use crate::keymap::Command;
use crate::palette::{
    Action, CancelOnDrop, Outcome, Page, PageRow, RowAction, nothing_found, open_page,
    open_palette_with, show_error, spawn_waking, start_busy,
};
use crate::session::Session;
use crate::workspace::{Project, Workspace};

/// A call to a plugin, which a row makes on Enter.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PluginCall {
    pub(crate) plugin: String,
    /// The contribution the plugin is invoked with.
    pub(crate) contribution: String,
    pub(crate) label: String,
    pub(crate) arg: Option<String>,
    pub(crate) subject: Option<Subject>,
    /// A page row's `data`, echoed back.
    pub(crate) data: Option<Value>,
    /// Shown while the call runs, which keeps the command centre busy. Without it, a page
    /// shows `loading` until the plugin answers.
    pub(crate) busy: Option<String>,
    pub(crate) loading: Option<String>,
    pub(crate) timeout: Duration,
}

impl PluginCall {
    /// A call to `contribution`, run as a contribution with these settings asks.
    fn new(
        plugin: &Plugin,
        contribution: &str,
        label: String,
        busy: &Option<String>,
        loading: &Option<String>,
        timeout: Option<&str>,
    ) -> Self {
        Self {
            plugin: plugin.name().to_owned(),
            contribution: contribution.to_owned(),
            label,
            arg: None,
            subject: None,
            data: None,
            busy: busy.clone(),
            loading: loading.clone(),
            timeout: plugin.timeout(timeout),
        }
    }
}

/// Finds the plugins in the bundled and user folders for every window.
pub(crate) fn load_plugins(state: &mut State) {
    let Some(paths) = &state.paths else { return };
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| benches_platform::dirs::bundled_plugins_dir(&exe));
    let dirs: Vec<PathBuf> = bundled.into_iter().chain([paths.plugins()]).collect();
    let config = &state.config;
    (state.fleet).load_plugins(&dirs, &config.disabled_plugins, config.shell.clone());
}

/// Whether the plugins or why some were left out changed since this window last showed them.
pub(crate) fn poll_plugins(state: &mut State) -> bool {
    let generation = state.fleet.poll_plugins();
    let changed = generation != state.plugins_shown;
    state.plugins_shown = generation;
    changed
}

fn holds(when: Option<When>, workspace: &Workspace, torn_off: bool) -> bool {
    match when {
        None => true,
        Some(When::Project) => workspace.project().is_some(),
        Some(When::TornOff) => torn_off,
        Some(When::NotTornOff) => !torn_off,
        Some(When::SeveralProjects) => workspace.projects.len() > 1,
    }
}

/// The rows of the prefixes `query` starts with.
pub(crate) fn prefix_actions(
    query: &str,
    plugins: &[Plugin],
    workspace: &Workspace,
    torn_off: bool,
) -> Vec<Action> {
    let project = workspace.project().map_or("", |project| &project.name);
    let mut actions = Vec::new();
    for plugin in plugins {
        for (prefix, arg) in plugin.prefixes(query) {
            if !holds(prefix.when, workspace, torn_off) {
                continue;
            }
            let template = match &prefix.label_with_arg {
                Some(template) if !arg.is_empty() => template,
                _ => &prefix.label,
            };
            let label = template
                .replace("{arg}", arg)
                .replace("{project.name}", project);
            actions.push(Action::Plugin(PluginCall {
                arg: Some(arg.to_owned()),
                ..PluginCall::new(
                    plugin,
                    &prefix.id,
                    label,
                    &prefix.busy,
                    &prefix.loading,
                    prefix.timeout.as_deref(),
                )
            }));
        }
    }
    actions
}

/// The plugin commands offered here, ranked with the built-in ones.
pub(crate) fn command_actions(
    plugins: &[Plugin],
    workspace: &Workspace,
    torn_off: bool,
) -> Vec<Action> {
    let mut actions = Vec::new();
    for plugin in plugins {
        for command in &plugin.manifest.commands {
            if !holds(command.when, workspace, torn_off) {
                continue;
            }
            let label = command.label.clone();
            // The manifest check leaves exactly one of `invoke` and `fill`.
            actions.push(match &command.invoke {
                Some(invoke) => Action::Plugin(PluginCall::new(
                    plugin,
                    invoke,
                    label,
                    &command.busy,
                    &command.loading,
                    command.timeout.as_deref(),
                )),
                None => {
                    let text = command.fill.clone().unwrap_or_default();
                    Action::Effects(label, vec![Effect::Fill { text }])
                }
            });
        }
    }
    actions
}

/// The first plugin's row action on `subject`, which goes before the built-in one.
pub(crate) fn row_action(plugins: &[Plugin], subject: &Subject) -> Option<RowAction> {
    let kind = match subject {
        Subject::Folder { .. } => SubjectKind::Folder,
    };
    plugins.iter().find_map(|plugin| {
        let action = plugin
            .manifest
            .row_actions
            .iter()
            .find(|action| action.subject == kind)?;
        Some(RowAction {
            hint: action.hint.clone(),
            action: Action::Plugin(PluginCall {
                subject: Some(subject.clone()),
                ..PluginCall::new(
                    plugin,
                    &action.id,
                    action.hint.clone(),
                    &action.busy,
                    &action.loading,
                    action.timeout.as_deref(),
                )
            }),
        })
    })
}

/// Runs the plugin off the UI thread, keeping the command centre busy, or showing a page that
/// loads, until it answers.
pub(crate) fn invoke(state: &mut State, call: &PluginCall, tx: &mut Tx) {
    let plugins = state.fleet.plugins();
    let Some(index) = plugins
        .iter()
        .position(|plugin| plugin.name() == call.plugin)
    else {
        show_error(
            state,
            &anyhow!("The {} plugin is no longer loaded", call.plugin),
        );
        return;
    };
    let request = Request {
        v: PROTOCOL_VERSION,
        id: plugins::next_request_id(),
        method: Method::Invoke,
        contribution: call.contribution.clone(),
        arg: call.arg.clone(),
        subject: call.subject.clone(),
        data: call.data.clone(),
        context: context(state),
    };
    let cancel = Cancel::default();
    let answer = spawn_waking(state.waker.clone(), {
        let (shell, call, cancel) = (state.config.shell.clone(), call.clone(), cancel.clone());
        move || {
            let answer = plugins::invoke(&shell, &plugins[index], &request, call.timeout, &cancel)?;
            outcome(answer, &call)
        }
    });
    match &call.busy {
        Some(label) => {
            let filter = call.arg.as_deref().unwrap_or_default();
            start_busy(state, label.clone(), filter, answer, cancel);
        }
        None => {
            let page = Page {
                loading: call
                    .loading
                    .clone()
                    .unwrap_or_else(|| "Loading…".to_owned()),
                pending: Some(answer),
                _cancel: CancelOnDrop(cancel),
                ..Page::loaded(nothing_found(), Vec::new())
            };
            open_page(state, page, call.arg.as_deref().unwrap_or_default(), tx);
        }
    }
}

fn context(state: &State) -> Context {
    let info = |project: &Project| ProjectInfo {
        root: project.root.clone(),
        name: project.name.clone(),
    };
    Context {
        project: state.workspace.project().map(info),
        cwd: state.session().and_then(Session::cwd),
        working_directory: state.config.working_directory.clone(),
        platform: std::env::consts::OS.to_owned(),
        projects: state.workspace.projects.iter().map(info).collect(),
    }
}

fn outcome(answer: Answer, call: &PluginCall) -> Result<Outcome> {
    match answer {
        Answer::Effects(effects) => Ok(Outcome::Run(Action::Effects(call.label.clone(), effects))),
        Answer::Rows(rows) => Ok(Outcome::Rows(
            rows.into_iter().map(|row| page_row(row, call)).collect(),
        )),
        Answer::Error(message) => Err(anyhow!(message)),
    }
}

/// A row of a page `call` answered, whose own invoke goes to the same plugin.
fn page_row(row: Row, call: &PluginCall) -> PageRow {
    let action = match row.enter {
        Enter::Effects(effects) => Action::Effects(row.label.clone(), effects),
        Enter::Invoke { invoke } => Action::Plugin(PluginCall {
            contribution: invoke,
            label: row.label.clone(),
            arg: None,
            data: row.data,
            ..call.clone()
        }),
    };
    PageRow {
        label: row.label,
        detail: row.detail,
        keywords: row.keywords,
        action,
    }
}

/// Runs `effects` in order, stopping at the first that fails to show why in the command centre.
pub(crate) fn run_effects(state: &mut State, effects: &[Effect], tx: &mut Tx) {
    for effect in effects {
        if let Err(error) = run_effect(state, effect, tx) {
            show_error(state, &error);
            return;
        }
    }
}

fn run_effect(state: &mut State, effect: &Effect, tx: &mut Tx) -> Result<()> {
    match effect {
        Effect::OpenProject { path } => open_project(state, path.clone()),
        Effect::OpenTab {
            title,
            command,
            cwd,
        } => {
            ensure!(
                state.workspace.project().is_some(),
                "No project to open a tab in"
            );
            open_tab(state, title, command.as_deref(), cwd.as_deref());
        }
        Effect::OpenTray { command } => open_tray(state, command),
        Effect::SwitchProject { root } => {
            let index = state
                .workspace
                .project_index(root)
                .with_context(|| format!("No open project at {}", root.display()))?;
            state.workspace.select_project(index);
        }
        Effect::RunCommand { id } => {
            let torn_off = state.is_torn_off();
            let spec = Command::from_id(id)
                .filter(|spec| (spec.enabled)(&state.workspace, &state.config, torn_off))
                .with_context(|| format!("No command “{id}” here"))?;
            run_command(state, spec.command, tx);
        }
        Effect::Fill { text } => open_palette_with(state, text, tx),
        Effect::OpenUrl { url } => {
            let link = Link::Url(url.clone());
            std::thread::spawn(move || benches_platform::open(&link).log_err("open url"));
        }
        Effect::Error { message } => bail!("{message}"),
        Effect::Close => state.palette.open = false,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use benches_core::config::{Config, Paths};
    use benches_core::test_support::stub_plugin;
    use rune::{Key, Mods, Runtime};

    use super::*;
    use crate::palette::palette_actions;
    use crate::test_support::*;

    const MANIFEST: &str = r#"
name = "stub"
exec = "run"

[[command]]
id = "list-command"
label = "Browse stub things"
invoke = "list"
loading = "Loading stub things…"

[[command]]
id = "fill"
label = "Fill stub"
fill = "stub "

[[command]]
id = "sidebar"
label = "Plugin sidebar"
invoke = "sidebar"
busy = "Toggling…"

[[prefix]]
id = "fail"
prefix = "fail "
arg = '^\S+$'
label = "Fail {arg}"
busy = "Failing…"

[[prefix]]
id = "find"
prefix = "find "
arg = '^\S+$'
label = "Find {arg}"
busy = "Finding…"

[[prefix]]
id = "slow"
prefix = "slow"
label = "Wait"
busy = "Waiting…"

[[prefix]]
id = "list"
prefix = "stub"
arg = '^(| .*)$'
label = "Stub things of {project.name}"
label_with_arg = "Stub things of {project.name} matching “{arg}”"
when = "project"

[[row_action]]
id = "list"
subject = "folder"
hint = "stub things"
"#;

    /// Answers by contribution, keeping the last request in `request.json` in its folder.
    const SCRIPT: &str = r#"read -r line
printf '%s\n' "$line" > request.json
id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
case "$line" in
*'"contribution":"list"'*)
    printf '{"id":%s,"rows":[{"label":"main","detail":"~/dev/main","enter":[{"open_project":{"path":"/dev/main"}}]},{"label":"feature","data":{"n":1},"enter":{"invoke":"open"}}]}\n' "$id" ;;
*'"contribution":"open"'*)
    printf '{"id":%s,"effects":[{"open_project":{"path":"/dev/feature"}}]}\n' "$id" ;;
*'"contribution":"fail"'*)
    printf '{"id":%s,"error":"✗ no such thing"}\n' "$id" ;;
*'"contribution":"sidebar"'*)
    printf '{"id":%s,"effects":[{"run_command":{"id":"toggle_sidebar"}}]}\n' "$id" ;;
*'"contribution":"find"'*)
    printf '{"id":%s,"rows":[{"label":"alpha","enter":["close"]},{"label":"find alpha","enter":["close"]}]}\n' "$id" ;;
*'"contribution":"slow"'*)
    echo $$ > pid
    sleep 30 ;;
esac"#;

    /// The stub plugin, loaded from the folder that holds it.
    fn stub() -> (tempfile::TempDir, Vec<Plugin>) {
        let dir = stub_plugin(MANIFEST, SCRIPT);
        let (plugins, errors) = plugins::discover(&[dir.path().to_owned()], &[]);
        assert!(errors.is_empty(), "{errors:?}");
        (dir, plugins)
    }

    fn last_request(dir: &tempfile::TempDir) -> Request {
        let request = std::fs::read_to_string(dir.path().join("stub/request.json")).unwrap();
        serde_json::from_str(&request).unwrap()
    }

    /// The command centre open with the stub plugin loaded.
    fn plugin_runtime() -> (tempfile::TempDir, Runtime<State>) {
        let (dir, plugins) = stub();
        let runtime = palette_runtime();
        runtime.state.fleet.set_plugins(plugins);
        (dir, runtime)
    }

    fn type_and_enter(runtime: &mut Runtime<State>, text: &str) {
        press(runtime, Key::Text(text.into()), Mods::default());
        press(runtime, Key::Enter, Mods::default());
    }

    fn wait_for_answer(runtime: &mut Runtime<State>) {
        frame_until(runtime, "the plugin to answer", |runtime| {
            let palette = &runtime.state.palette;
            palette.busy.is_none()
                && palette
                    .page
                    .as_ref()
                    .is_none_or(|page| page.pending.is_none())
        });
    }

    fn label(action: &Action) -> String {
        action.label(Path::new("/"))
    }

    #[test]
    fn a_prefix_offers_its_row_first_with_its_label_filled_in() {
        let (_dir, plugins) = stub();
        let actions = |query, workspace: &Workspace| {
            palette_actions(query, None, workspace, &Config::default(), false, &plugins)
        };
        let workspace = one_project();
        assert_eq!(
            label(&actions("stub", &workspace)[0]),
            "Stub things of benches"
        );
        assert_eq!(
            label(&actions("stub fe", &workspace)[0]),
            "Stub things of benches matching “fe”"
        );
        assert!(
            !actions("stubx", &workspace)
                .iter()
                .any(|action| matches!(action, Action::Plugin(_)))
        );
        let without_project = actions("stub", &Workspace::default());
        assert!(
            without_project
                .iter()
                .all(|action| !label(action).starts_with("Stub things")),
            "{without_project:?}"
        );
    }

    #[test]
    fn plugin_commands_rank_with_the_built_in_ones_once_something_is_typed() {
        let (_dir, plugins) = stub();
        let actions = |query| {
            palette_actions(
                query,
                None,
                &one_project(),
                &Config::default(),
                false,
                &plugins,
            )
        };
        assert_eq!(label(&actions("browse")[0]), "Browse stub things");
        assert!(
            actions("")
                .iter()
                .all(|action| !matches!(action, Action::Plugin(_)))
        );
    }

    #[test]
    fn a_page_from_a_plugin_filters_and_enter_runs_a_rows_effects() {
        let (_dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "stub ma");
        assert_eq!(runtime.state.palette.edit.text, "ma");
        assert!(runtime.find("palette-0").is_some());
        wait_for_answer(&mut runtime);
        let shown: Vec<String> = runtime
            .state
            .palette
            .page
            .iter()
            .flat_map(|page| &page.rows)
            .map(|row| row.label.clone())
            .collect();
        assert_eq!(shown, ["main", "feature"]);
        press(&mut runtime, Key::Enter, Mods::default());
        let workspace = &runtime.state.workspace;
        assert_eq!(
            workspace.projects.last().unwrap().root,
            Path::new("/dev/main")
        );
        assert!(!runtime.state.palette.open);
    }

    #[test]
    fn a_row_that_invokes_the_plugin_sends_its_data_back() {
        let (dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "stub feature");
        wait_for_answer(&mut runtime);
        press(&mut runtime, Key::Enter, Mods::default());
        wait_for_answer(&mut runtime);
        let workspace = &runtime.state.workspace;
        assert_eq!(
            workspace.projects.last().unwrap().root,
            Path::new("/dev/feature")
        );
        let request = last_request(&dir);
        assert_eq!(request.contribution, "open");
        assert_eq!(request.data, Some(serde_json::json!({"n": 1})));
        assert_eq!(request.context.project.unwrap().name, "benches");
    }

    #[test]
    fn a_plugin_error_shows_in_the_command_centre() {
        let (_dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "fail x");
        assert!(runtime.state.palette.busy.is_some());
        wait_for_answer(&mut runtime);
        runtime.frame(1000.0);
        assert!(runtime.state.palette.open);
        assert_eq!(
            runtime.state.palette.error.as_deref(),
            Some("✗ no such thing")
        );
        assert!(runtime.find("palette-error").is_some());
    }

    #[test]
    fn escape_ends_a_busy_plugin() {
        let (dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "slow");
        assert!(runtime.find("palette-0").is_some());
        let pid = wait_for_pid(&dir.path().join("stub/pid"));
        press(&mut runtime, Key::Escape, Mods::default());
        assert!(runtime.state.palette.busy.is_none());
        assert!(!runtime.state.palette.open);
        wait_for_pid_exit(pid, Duration::from_secs(15), "plugin");
    }

    #[test]
    fn rows_from_a_busy_call_open_filtered_by_its_argument_with_the_top_row_selected() {
        let (_dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "find al");
        assert!(runtime.state.palette.busy.is_some());
        wait_for_answer(&mut runtime);
        runtime.frame(1000.0);
        assert_eq!(runtime.state.palette.edit.text, "al");
        assert_eq!(runtime.state.palette.selected, 0);
    }

    #[test]
    fn a_fill_command_types_into_the_command_centre() {
        let (_dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "fill stub");
        assert!(runtime.state.palette.open);
        assert_eq!(runtime.state.palette.edit.text, "stub ");
    }

    #[test]
    fn a_plugin_runs_built_in_commands_by_id() {
        let (_dir, mut runtime) = plugin_runtime();
        type_and_enter(&mut runtime, "plugin sidebar");
        assert!(runtime.state.palette.busy.is_some());
        wait_for_answer(&mut runtime);
        assert!(runtime.state.sidebar_hidden);
        let mut state = workspace_state();
        run_effects(
            &mut state,
            &[Effect::RunCommand {
                id: "merge_into_main".to_owned(),
            }],
            &mut Tx::default(),
        );
        assert_eq!(
            state.palette.error.as_deref(),
            Some("No command “merge_into_main” here")
        );
    }

    #[test]
    fn a_plugin_row_action_takes_command_enter_on_the_jump_folder() {
        let (dir, mut runtime) = plugin_runtime();
        press(&mut runtime, Key::Text("site".into()), Mods::default());
        runtime.state.palette.jump.query = "site".to_owned();
        runtime.state.palette.jump.found = Some(PathBuf::from("/dev/site"));
        runtime.frame(0.0);
        assert!(runtime.find("palette-alternative").is_some());
        press(&mut runtime, Key::Enter, command());
        wait_for_answer(&mut runtime);
        assert_eq!(runtime.state.palette.page.as_ref().unwrap().rows.len(), 2);
        let request = last_request(&dir);
        assert_eq!(
            request.subject,
            Some(Subject::Folder {
                path: PathBuf::from("/dev/site")
            })
        );
    }

    #[test]
    fn plugins_load_at_launch_and_reload_config_rescans_them() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::under(root.path());
        let user = paths.plugins();
        std::fs::create_dir_all(user.join("stub")).unwrap();
        std::fs::write(user.join("stub/plugin.toml"), MANIFEST).unwrap();
        std::fs::create_dir_all(user.join("broken")).unwrap();
        std::fs::write(user.join("broken/plugin.toml"), "name = ").unwrap();
        let mut state = workspace_state();
        state.paths = Some(paths.clone());
        crate::windows::load_config(&mut state);
        let mut runtime = runtime_with(state);
        frame_until(&mut runtime, "plugins to load", |runtime| {
            runtime.state.fleet.plugins().len() == 1
        });
        let error = runtime.state.fleet.plugin_error().unwrap();
        assert!(error.contains("broken/plugin.toml"), "{error}");
        assert!(runtime.find("plugin-error").is_some());
        std::fs::write(
            root.path().join("benches/config.toml"),
            "[plugins]\ndisabled = [\"stub\"]\n",
        )
        .unwrap();
        runtime.update(|state, _| crate::windows::reload_config(state));
        frame_until(&mut runtime, "the disabled plugin to go", |runtime| {
            runtime.state.fleet.plugins().is_empty()
        });
    }
}
