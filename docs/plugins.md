# Command palette plugins

Status: design, not implemented.

Plugins are separate executables described by a `plugin.toml` manifest and spoken to in JSON Lines. They return rows and effects, and the palette stays the only code that touches UI. Worktrunk (`wt`, `wts`, ⌘↵ worktrees on a jump folder) becomes the first plugin, with no visible change.

## Context

The command centre (`src/palette.rs`) hard-codes every source of rows: `COMMANDS` in `src/keymap.rs`, project/tab/tray switches, the jump command's folder, `!` shell history, and worktrunk.

Worktrunk alone threads through `Action::CreateWorktree`/`ListWorktrees`, `Command::OpenWorktree`/`CreateWorktree`, `WorktreeChoice`, `Creation`, `poll_worktrees`, `poll_creation`, `worktree_key` and half of `src/jump.rs`. Each new integration would repeat that spread. The goal is integrations loaded at runtime, hooking both palette queries and rows such as the jump command's folder.

## Decisions

| Question | Decision | Why |
| --- | --- | --- |
| Runtime | External process: a manifest plus any executable | Extends `process::run` (login shell, process group, timeout, `Cancel`); crash-isolated; any language |
| Wire format | JSON Lines on stdin/stdout | Spawn cost dominates; scripts and `wt` already speak JSON (see [Alternatives](#alternatives-considered)) |
| Plugin powers | Return data only: rows and effects | The host runs a closed effect vocabulary on the UI thread; plugins never touch `State` |
| When a process runs | Only on invoke or a dynamic lookup | Commands, template prefix rows and row-action labels come from the manifest |

## Concepts

A plugin adds **contributions** to the palette; the host owns the rows and runs the results.

| Contribution | What it adds | Process? | Today's equivalent |
| --- | --- | --- | --- |
| `command` | A static palette command, fuzzy-ranked with `COMMANDS`; `fill` prefills the input or `invoke` runs the plugin | Only with `invoke` | Open worktree…, Create worktree… |
| `prefix` | Claims a query prefix. A template row (label + `arg` pattern) or a dynamic, debounced `query` | Template: on Enter only | `wt <branch>`, `wts [filter]` |
| `row_action` | A secondary action on a host **subject** row, bound to ⌘↵ or shown as an alternative | On invoke | ⌘↵ worktrees on a jump folder |
| `provider` (phase 4) | A debounced search source merged into ordinary queries, yielding subjects | Per lookup | The jump command |

- **Subject**: a typed row the host owns, passed to plugins as context: `folder{path}`, `project{index,root,name}`, `tab`, `tray`.
- **Page**: a sub-list pushed by an invoke that returns rows, filtered on its visible labels as the user types (see [Filtering pages](#filtering-pages)); Esc pops. Generalises `WorktreeChoice`.
- **Busy**: a long-running invoke showing the manifest's `busy` text, cancelled with Esc. Generalises `Creation`.
- **Effect**: what the host does with a result: `open_project{path}`, `open_tab{title,command?,cwd?}`, `open_tray{command}`, `switch_project{root}`, `run_command{id}`, `fill{text}`, `open_url{url}`, `error{message}`, `close`.

### Filtering pages

A page filters on what each row shows, so every keystroke visibly narrows the list. Today's worktree list matches the branch plus the full path while showing a `~` path, so the hidden `/Users/<name>` letters keep rows that don't visibly match.

- **Host-side by default**: each keystroke ranks the page's rows with `fuzzy_match`, the scorer commands use, against the row's `label` and optional `detail`. Rows that don't match are hidden, best match first, with no round trip to the plugin.
- **Highlighting**: matched characters are drawn emphasised, so it's clear why a row stayed.
- **`keywords`**: a row may list extra words that match but aren't shown, such as a full path. A keyword-only match ranks below any label match.
- **`"filter":"plugin"`**: a page too large to send whole asks the host to send a debounced `query` with the typed text instead. It reuses the `Lookup` the jump command uses.
- **Selection**: it stays on the same row while that row still matches, else returns to the top, as `poll_selection` does for the main list.

## Manifest

Everything the palette shows before Enter comes from `plugin.toml`, so typing `wt feat` spawns nothing. Worktrunk's manifest reproduces today's behaviour:

```toml
name = "worktrunk"
description = "Git worktrees through Worktrunk's `wt`"
exec = "benches-worktrunk"   # relative to the plugin dir, else resolved by the login shell
requires = ["wt"]            # hidden unless `command -v wt` succeeds (checked once, cached)

[[command]]
id = "open"
label = "Open worktree…"
when = "project"
invoke = "list"              # pushes a page from `invoke list` on the active project

[[command]]
id = "create"
label = "Create worktree…"
when = "project"
fill = "wt "                 # no process; prefills the input

[[prefix]]
id = "create"
prefix = "wt "
arg = '^[^-\s]\S*$'          # one branch name wt can't read as an option
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
```

`when` is a small fixed set (`project`, `torn_off`, `!torn_off`, `projects>1`) mirroring `CommandSpec::enabled`. Unknown keys are errors, and a JSON schema is generated as `config schema` does.

## Protocol v1

Two methods, `invoke` and `query`, plus a `cancel` notification; every response answers one `id` with rows, effects or an error.

Host to plugin, one JSON object per line:

```json
{"v":1,"id":7,"method":"invoke","contribution":"create","arg":"feature-x","subject":null,"context":{}}
{"v":1,"id":8,"method":"query","contribution":"list","query":"fe","context":{}}
{"v":1,"method":"cancel","id":7}
```

`context` carries the active project `{root,name}`, the active tab's cwd (`Session::cwd`), `working_directory`, the platform and the open projects.

Plugin to host:

```json
{"id":7,"effects":[{"open_project":{"path":"/dev/site.feature-x"}}]}
{"id":7,"error":"✗ Branch feature-x already exists"}
{"id":9,"rows":[{"label":"main  ~/dev/site","data":{"path":"/dev/site"},"enter":[{"open_project":{"path":"/dev/site"}}]}]}
```

A row's `enter` or `alt` is either an inline effect list (no round trip) or `{"invoke":"<contribution>"}`, with the row's `data` echoed back. Today's `fallback_to_root` moves into the plugin: `list` on a folder that isn't a repository returns `open_project` on that folder instead of rows.

## Process model

Plugin calls never block the UI thread: requests run on worker threads and answers come back over mpsc plus the window's `Waker`, as `spawn_waking` does today.

```
palette (UI thread) --invoke/query--> PluginHost --> worker thread --> plugin process
        ^                                                                  |
        +---- rows / effects / error over mpsc + Waker; poll runs effects -+
```

- **oneshot** (default, phase 2): spawn per request through the login shell, request on stdin, response on stdout, exit. Cancel kills the process group, as `process::Cancel` does.
- **persistent** (phase 4, `mode = "persistent"`): spawned lazily once per app, requests multiplexed by `id`, `cancel` messages, restarted on exit, killed on Reload Config. It removes the login-shell start per call: `fish -l -i -c true` measured 60–170 ms, `wt list` 250 ms end to end.
- **Timeouts**: the manifest's `timeout`, defaulting to 5 s like `JUMP_TIMEOUT`.
- **Sharing**: one `PluginHost` per app, shared across windows through an `Arc` as `Fleet` is. Stale responses are dropped by `id`.

## Where it lives

The host goes in `benches-core` so it stays free of Rune; only the palette glue sits in the app.

| Location | Holds |
| --- | --- |
| `crates/benches-plugin` (new, `plugin.rs` root) | Protocol and manifest types, serde only; shared by the host and Rust plugins |
| `benches-core::process` | `run`, `login_args`, `Cancel`, `strip_escapes`, moved out of `src/jump.rs`, with errors naming the command |
| `benches-core::plugins` | Discovery, manifest validation, `requires` probing, oneshot and persistent runners |
| `src/palette.rs` | `Action::Plugin{plugin, contribution, row}`; `Page` and `Busy` replacing `WorktreeChoice` and `Creation`; one debounced `Lookup` replacing `JumpLookup`'s own debounce; ranking bands |
| `plugins/worktrunk/` (new binary `benches-worktrunk`) | The `wt list` and `wt switch --create` code and tests from `jump.rs` |

**Ranking** keeps today's order: prefix rows first, as `wt` and `wts` are now; plugin commands fuzzy-ranked with `COMMANDS`; row actions as ⌘↵ hints.

**Discovery**: bundled plugins first (`Benches.app/Contents/Resources/plugins`, or `PREFIX/share/benches/plugins` on Linux beside terminfo, through `dirs.rs`), then `~/.config/benches/plugins/*`. A user plugin with the same name overrides a bundled one. `plugins.disabled = ["worktrunk"]` in the config turns one off, and Reload Config rescans.

**Trust**: plugins run with the user's privileges, like their shell config. Arguments go in as argv, never spliced into a script, as `login_args` does. No sandbox, and the docs say so.

## Alternatives considered

### Protobuf instead of JSON

Rejected: the cost is the process, not the encoding.

- Process and login-shell start take 60–250 ms. Encoding a few hundred rows as JSON takes microseconds, so protobuf saves nothing visible.
- JSON can be written from a shell script with `jq`, read in logs, and `wt` already emits it. Protobuf needs codegen per language and length-prefixed framing that scripts can't easily produce.
- The real speed lever is persistent mode, which works with any format. `"v":1` on every message leaves room for a binary framing if a measured need appears.

### C-ABI dynamic libraries

Rejected. A C ABI (`extern "C"`, `#[repr(C)]`, or `abi_stable`/`stabby`) does survive Rust compiler changes, unlike a C++ ABI, but it loses here:

- **No speed gain.** Palette plugins mostly shell out, so in-process calls save microseconds and still need a worker thread.
- **Crash blast radius.** A segfault, UB or a panic across FFI aborts the app and every terminal session in every window. A crashed process is an error line.
- **Breaks the fd rule.** Every fd the app opens must be close-on-exec from the start, or shells spawned by other threads inherit it. In-process plugin code can't be held to that.
- **macOS signing.** Notarization needs the hardened runtime, whose library validation refuses dylibs signed by another team unless the app has `disable-library-validation`. Downloaded dylibs are also quarantined. `scripts/bundle` doesn't enable the hardened runtime today.
- **Distribution and lifecycle.** Builds per OS, architecture and glibc; unsafe glue; and `dlclose` is unsafe in practice, so Reload Config couldn't reload plugins.

If a hot-path hook (per frame, per keystroke, output filters) is ever needed, WASM is the in-process option: a stable ABI, contained faults and no ambient fds. Contributions and effects don't depend on the transport, so it could be added without changing the palette.

## Phases

Five PRs; each lands its behaviour-preserving refactor as its own commit first.

1. **Refactor, no behaviour change**: move the process helpers to `benches-core::process`; generalise `WorktreeChoice` into `Page`, `Creation` into `Busy` and the jump debounce into `Lookup`; give the jump row's ⌘↵ a row-action slot.
2. **Host and oneshot**: the `benches-plugin` crate, manifest loading, `command`, `prefix` and `row_action`, effects, errors in the palette's error line, and tests against a stub plugin built with `test_support::stub_shell`.
3. **Worktrunk as a plugin**: the `plugins/worktrunk` binary, bundled by `scripts/bundle` and the Linux install. Delete `Command::OpenWorktree`/`CreateWorktree`, `Action::CreateWorktree`/`ListWorktrees` and the wt code in `jump.rs`, with the same visible behaviour.
4. **Persistent mode, dynamic prefixes and `provider`**, for example a zoxide folder provider.
5. **Docs for third-party authors**: a protocol reference and an example plugin in shell and `jq`.

## Open questions

- Keybindings for plugin commands: a config `[keys]` table mapping chords to `plugin:command`?
- Project-local plugins (`.benches/plugins` in a repo) need a trust prompt; out of v1?
- Hooks beyond the palette (tab lifecycle, sidebar badges, notifications) are out of scope; subjects and effects are meant to extend to them.
- Should `!` shell history and the jump command become built-in plugins? Proposed: both stay native, and the jump command emits `folder` subjects that plugins extend.
