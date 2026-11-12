use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use libghostty_vt::color::Rgb;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::Error as ValueError;

use crate::LogErr;
use crate::constraint::Modifier;
use crate::grid::{DEFAULT_BG, DEFAULT_FG};

const DEFAULT_SHELL: &str = "/opt/homebrew/bin/fish";
pub const DEFAULT_FONT_FAMILY: &str = "Monaspace Neon";
const DEFAULT_FONT_SIZE: f64 = 18.0;
const GHOSTTY_THEMES: &str = "/Applications/Ghostty.app/Contents/Resources/ghostty/themes";

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub shell: PathBuf,
    pub font: FontConfig,
    pub theme: Theme,
    /// The tabs a new project opens with.
    pub tabs: Vec<TabSpec>,
    /// `jump-command`: a command such as `j` or `zoxide query` that the command centre runs
    /// through `shell` with the query to find a folder to open as a project.
    pub jump_command: Option<String>,
    /// `auto-secure-input`: turn secure keyboard entry on while a program reads a password.
    pub auto_secure_input: bool,
    /// Whether new fish shells are started with prompt marks and directory reports.
    pub shell_integration: bool,
    pub clipboard_read: ClipboardAccess,
    pub clipboard_write: ClipboardAccess,
}

/// Ghostty's `clipboard-read` and `clipboard-write` values.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardAccess {
    Allow,
    Deny,
    Ask,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    pub size: f64,
    /// The face for private-use icons; `None` tries `fallback_families`, then the main font.
    pub symbols_family: Option<String>,
    /// Ghostty's `font-family` lines after the first.
    pub fallback_families: Vec<String>,
    /// Ghostty's `font-feature` syntax: `calt`, `-liga`, `ss01=2`.
    pub features: Vec<String>,
    /// Ghostty's `font-shaping-break = cursor`: the cursor splits a ligature.
    pub shaping_breaks_at_cursor: bool,
    /// Ghostty's `adjust-icon-height`: the icons' maximum height, by a percentage or in pixels.
    pub adjust_icon_height: Option<Modifier>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq)]
pub struct TabSpec {
    /// The tab's label.
    pub title: String,
    /// The command to run through the launch shell; omit to open the shell itself.
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<Rgb>,
    pub selection_background: Option<Rgb>,
    pub selection_foreground: Option<Rgb>,
    /// Overrides of the built-in 256-colour palette.
    pub palette: Vec<(u8, Rgb)>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: DEFAULT_FG,
            background: DEFAULT_BG,
            cursor: None,
            selection_background: None,
            selection_foreground: None,
            palette: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            shell: PathBuf::from(DEFAULT_SHELL),
            font: FontConfig {
                family: DEFAULT_FONT_FAMILY.to_owned(),
                size: DEFAULT_FONT_SIZE,
                symbols_family: None,
                fallback_families: Vec::new(),
                features: Vec::new(),
                shaping_breaks_at_cursor: true,
                adjust_icon_height: None,
            },
            theme: Theme::default(),
            tabs: vec![
                TabSpec {
                    title: "claude".to_owned(),
                    command: Some("claude".to_owned()),
                },
                TabSpec {
                    title: "shell".to_owned(),
                    command: None,
                },
            ],
            jump_command: None,
            auto_secure_input: true,
            shell_integration: true,
            clipboard_read: ClipboardAccess::Ask,
            clipboard_write: ClipboardAccess::Allow,
        }
    }
}

/// The `config.toml` format. Every key is optional.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct File {
    /// The shell new tabs run, as an absolute path.
    shell: Option<PathBuf>,
    /// A Ghostty theme name or absolute path; `light:X,dark:Y` picks the dark one.
    /// Falls back to Ghostty's theme.
    theme: Option<String>,
    /// The terminal font. Each key falls back to Ghostty's config.
    #[serde(default)]
    font: FontFile,
    /// The tabs a new project opens with.
    tabs: Option<Vec<TabSpec>>,
    /// A command such as `j` or `zoxide query` that the command centre runs through `shell`
    /// with the query to find a folder to open as a project.
    #[serde(rename = "jump-command")]
    jump_command: Option<String>,
    /// Whether to turn on secure keyboard entry while a program reads a password. Falls back to
    /// Ghostty's `macos-auto-secure-input`, then on.
    #[serde(rename = "auto-secure-input")]
    auto_secure_input: Option<bool>,
    /// `none` starts shells as they are; anything else injects shell integration, which makes
    /// fish report its prompts and directory. Falls back to Ghostty's setting.
    #[serde(rename = "shell-integration")]
    shell_integration: Option<String>,
    /// Whether programs may read the clipboard through OSC 52: `allow`, `deny` or `ask`
    /// (the default). Falls back to Ghostty's setting.
    #[serde(rename = "clipboard-read")]
    clipboard_read: Option<ClipboardAccess>,
    /// Whether programs may set the clipboard through OSC 52: `allow` (the default), `deny` or
    /// `ask`. Falls back to Ghostty's setting.
    #[serde(rename = "clipboard-write")]
    clipboard_write: Option<ClipboardAccess>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FontFile {
    /// The font family. Falls back to Ghostty's first `font-family`.
    family: Option<String>,
    /// The font size in points. Falls back to Ghostty's `font-size`.
    size: Option<f64>,
    /// The face for private-use icons; unset tries Ghostty's other `font-family` lines, then
    /// the main font.
    #[serde(rename = "symbols-family")]
    symbols_family: Option<String>,
    /// OpenType features in Ghostty's `font-feature` syntax, such as `-calt` or `ss01`; replaces
    /// Ghostty's list, and `[]` turns them all off.
    features: Option<Vec<String>>,
    /// Where to split ligatures, in Ghostty's `font-shaping-break` syntax: `cursor` (the default)
    /// or `no-cursor`. Replaces Ghostty's setting.
    #[serde(rename = "shaping-break")]
    shaping_break: Option<Vec<String>>,
    /// The maximum height of icons, in Ghostty's `adjust-icon-height` syntax: a percentage such as
    /// `-10%` or a pixel change such as `-2`. Falls back to Ghostty's setting.
    #[serde(rename = "adjust-icon-height")]
    adjust_icon_height: Option<String>,
}

/// Where to look for the files the config names.
#[derive(Clone)]
pub struct Paths {
    /// Holds `config.toml`, `state.json` and a `themes` folder.
    pub dir: PathBuf,
    pub ghostty_dir: PathBuf,
}

impl Paths {
    pub fn from_home() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        Some(Self {
            dir: home.join(".config/agentd"),
            ghostty_dir: home.join(".config/ghostty"),
        })
    }

    pub fn state(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }

    fn schema(&self) -> PathBuf {
        self.dir.join("config.schema.json")
    }
}

const SCHEMA_DIRECTIVE: &str = "#:schema ./config.schema.json";

pub fn schema() -> String {
    format!("{:#}\n", schemars::schema_for!(File).as_value())
}

/// `agentd config init|migrate|schema`, run before the app starts.
pub fn command(args: &[String], paths: &Paths, out: &mut impl std::io::Write) -> Result<()> {
    match args {
        [name] if name == "init" => init(paths),
        [name] if name == "migrate" => migrate(paths),
        [name] if name == "schema" => write!(out, "{}", schema()).context("print schema"),
        _ => bail!("usage: agentd config init|migrate|schema"),
    }
}

fn write_schema(paths: &Paths) -> Result<()> {
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("create {}", paths.dir.display()))?;
    std::fs::write(paths.schema(), schema())
        .with_context(|| format!("write {}", paths.schema().display()))
}

fn init(paths: &Paths) -> Result<()> {
    if paths.config().exists() {
        bail!(
            "{} already exists; run `agentd config migrate` to update it",
            paths.config().display()
        );
    }
    write_schema(paths)?;
    std::fs::write(paths.config(), default_config_text())
        .with_context(|| format!("write {}", paths.config().display()))
}

/// Edits the first line as text, which keeps every comment and the key order as written.
fn migrate(paths: &Paths) -> Result<()> {
    let text = read_optional(&paths.config())?
        .with_context(|| format!("no {}; run `agentd config init`", paths.config().display()))?;
    text.parse::<toml_edit::DocumentMut>()
        .context("parse config.toml")?;
    write_schema(paths)?;
    let body = match text.split_once('\n') {
        Some((first, rest)) if first.starts_with("#:schema") => rest,
        _ => &text,
    };
    std::fs::write(paths.config(), format!("{SCHEMA_DIRECTIVE}\n{body}"))
        .with_context(|| format!("write {}", paths.config().display()))
}

fn default_config_text() -> String {
    use toml_edit::Value;
    let defaults = Config::default();
    let shell = Value::from(defaults.shell.to_string_lossy().as_ref());
    let family = Value::from(defaults.font.family.as_str());
    let tabs: String = defaults
        .tabs
        .iter()
        .map(|tab| {
            let command = tab.command.as_deref().map_or_else(
                || "# command = \"...\"\n".to_owned(),
                |command| format!("command = {}\n", Value::from(command)),
            );
            format!(
                "\n[[tabs]]\ntitle = {}\n{command}",
                Value::from(tab.title.as_str())
            )
        })
        .collect();
    format!(
        "{SCHEMA_DIRECTIVE}\n\
         # The shell new tabs run.\n\
         shell = {shell}\n\
         # A Ghostty theme name or absolute path; unset uses Ghostty's theme.\n\
         # theme = \"dusk\"\n\
         # A command the command centre runs through `shell` with the query to find a folder.\n\
         # jump-command = \"zoxide query\"\n\
         # Turn on secure keyboard entry while a program reads a password; unset uses Ghostty's\n\
         # `macos-auto-secure-input`, then true.\n\
         # auto-secure-input = false\n\
         # `none` starts shells as they are, without prompt marks or directory reports; unset uses\n\
         # Ghostty's `shell-integration`.\n\
         # shell-integration = \"none\"\n\
         # Whether programs may read or set the clipboard through OSC 52: \"allow\", \"deny\" or\n\
         # \"ask\". Unset uses Ghostty's, then \"ask\" for reads and \"allow\" for writes.\n\
         # clipboard-read = \"ask\"\n\
         # clipboard-write = \"allow\"\n\
         \n\
         [font]\n\
         # family = {family}\n\
         # size = {size:?}\n\
         # The face for private-use icons.\n\
         # symbols-family = \"Symbols Nerd Font Mono\"\n\
         # OpenType features in Ghostty's `font-feature` syntax; unset uses Ghostty's.\n\
         # features = [\"-calt\", \"ss01\"]\n\
         # Where to split ligatures, in Ghostty's `font-shaping-break` syntax: `cursor` (the\n\
         # default) shows the characters under the cursor; `no-cursor` keeps the ligature.\n\
         # shaping-break = [\"no-cursor\"]\n\
         # The maximum height of icons, as a percentage or in pixels, like Ghostty's\n\
         # `adjust-icon-height`; unset uses Ghostty's.\n\
         # adjust-icon-height = \"-10%\"\n\
         {tabs}",
        size = defaults.font.size,
    )
}

impl Config {
    /// `config.toml`, then Ghostty's config for the font and theme it leaves out, then the defaults.
    /// Problems are logged and fall back, so the app always starts.
    pub fn load(paths: &Paths) -> Self {
        let file = read_optional(&paths.dir.join("config.toml"))
            .and_then(|text| {
                text.map(|text| toml::from_str::<File>(&text).context("parse config.toml"))
                    .transpose()
            })
            .log_err("config")
            .flatten()
            .unwrap_or_default();
        let ghostty = read_optional(&paths.ghostty_dir.join("config"))
            .log_err("ghostty config")
            .flatten()
            .unwrap_or_default();
        let ghostty_value = |key: &str| {
            ghostty_values(&ghostty, key)
                .last()
                .map(|value| (*value).to_owned())
        };
        let ghostty_families = ghostty_values(&ghostty, "font-family");
        let defaults = Config::default();
        let ghostty_size = ghostty_value("font-size")
            .and_then(|size| size.parse::<f64>().log_err("ghostty font-size"));
        let clipboard = |setting: Option<ClipboardAccess>, key: &str, default| {
            setting
                .or_else(|| {
                    ghostty_value(key).and_then(|value| {
                        ClipboardAccess::deserialize(
                            IntoDeserializer::<ValueError>::into_deserializer(value.as_str()),
                        )
                        .log_err(key)
                    })
                })
                .unwrap_or(default)
        };
        let theme = file
            .theme
            .or_else(|| ghostty_value("theme"))
            .and_then(|name| load_theme(&name, paths).log_err("theme"))
            .unwrap_or_default();
        Self {
            shell: file.shell.unwrap_or(defaults.shell),
            font: FontConfig {
                // Ghostty's later `font-family` lines are fallbacks; the first is the face.
                family: file
                    .font
                    .family
                    .or_else(|| ghostty_families.first().map(|family| (*family).to_owned()))
                    .unwrap_or(defaults.font.family),
                size: file
                    .font
                    .size
                    .or(ghostty_size)
                    .unwrap_or(defaults.font.size),
                symbols_family: file.font.symbols_family,
                fallback_families: ghostty_families
                    .iter()
                    .skip(1)
                    .map(|family| (*family).to_owned())
                    .collect(),
                features: file.font.features.unwrap_or_else(|| {
                    ghostty_values(&ghostty, "font-feature")
                        .into_iter()
                        .map(str::to_owned)
                        .collect()
                }),
                shaping_breaks_at_cursor: shaping_breaks_at_cursor(
                    &file.font.shaping_break.unwrap_or_else(|| {
                        ghostty_values(&ghostty, "font-shaping-break")
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    }),
                ),
                adjust_icon_height: file
                    .font
                    .adjust_icon_height
                    .or_else(|| ghostty_value("adjust-icon-height"))
                    .and_then(|value| value.parse().log_err("adjust-icon-height")),
            },
            theme,
            tabs: file.tabs.unwrap_or(defaults.tabs),
            jump_command: file.jump_command,
            auto_secure_input: file
                .auto_secure_input
                .or_else(|| {
                    ghostty_value("macos-auto-secure-input")
                        .and_then(|value| value.parse().log_err("macos-auto-secure-input"))
                })
                .unwrap_or(defaults.auto_secure_input),
            shell_integration: file
                .shell_integration
                .or_else(|| ghostty_value("shell-integration"))
                .is_none_or(|value| value != "none"),
            clipboard_read: clipboard(
                file.clipboard_read,
                "clipboard-read",
                defaults.clipboard_read,
            ),
            clipboard_write: clipboard(
                file.clipboard_write,
                "clipboard-write",
                defaults.clipboard_write,
            ),
        }
    }
}

pub fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// Every value of `key` in a Ghostty-style `key = value` file, in order.
fn ghostty_values<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines()
        .filter_map(|line| line.trim().split_once('='))
        .filter(|(name, _)| name.trim() == key)
        .map(|(_, value)| value.trim().trim_matches('"'))
        .collect()
}

/// Ghostty's `font-shaping-break`: comma-separated options, where `no-` turns one off, and the
/// cursor is on unless a later mention says otherwise.
fn shaping_breaks_at_cursor(values: &[String]) -> bool {
    values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .fold(true, |breaks, option| match option {
            "cursor" => true,
            "no-cursor" => false,
            _ => breaks,
        })
}

/// A theme name as Ghostty takes it; `light:X,dark:Y` picks the dark one, as agentd is dark.
fn load_theme(name: &str, paths: &Paths) -> Result<Theme> {
    let name = name
        .split(',')
        .find_map(|part| part.trim().strip_prefix("dark:"))
        .unwrap_or(name)
        .trim();
    let path = if Path::new(name).is_absolute() {
        PathBuf::from(name)
    } else {
        [
            paths.dir.join("themes"),
            paths.ghostty_dir.join("themes"),
            PathBuf::from(GHOSTTY_THEMES),
        ]
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .with_context(|| format!("no theme named {name}"))?
    };
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse_theme(&text).with_context(|| format!("parse {}", path.display()))
}

fn parse_theme(text: &str) -> Result<Theme> {
    let color = |key: &str| {
        ghostty_values(text, key)
            .last()
            .map(|value| parse_color(value))
            .transpose()
    };
    let mut theme = Theme::default();
    theme.foreground = color("foreground")?.unwrap_or(theme.foreground);
    theme.background = color("background")?.unwrap_or(theme.background);
    theme.cursor = color("cursor-color")?;
    theme.selection_background = color("selection-background")?;
    theme.selection_foreground = color("selection-foreground")?;
    for entry in ghostty_values(text, "palette") {
        let (index, value) = entry
            .split_once('=')
            .with_context(|| format!("palette entry {entry}"))?;
        theme
            .palette
            .push((index.trim().parse()?, parse_color(value)?));
    }
    Ok(theme)
}

fn parse_color(value: &str) -> Result<Rgb> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        bail!("colour {value} isn't #rrggbb");
    }
    let channel =
        |range| u8::from_str_radix(&hex[range], 16).with_context(|| format!("colour {value}"));
    Ok(Rgb {
        r: channel(0..2)?,
        g: channel(2..4)?,
        b: channel(4..6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(config: Option<&str>, ghostty: Option<&str>) -> (tempfile::TempDir, Paths) {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: root.path().join("agentd"),
            ghostty_dir: root.path().join("ghostty"),
        };
        for (dir, text) in [(&paths.dir, config), (&paths.ghostty_dir, ghostty)] {
            std::fs::create_dir_all(dir.join("themes")).unwrap();
            std::fs::write(
                dir.join("themes/dusk"),
                "palette = 1=#ff0000\nbackground = #101010\nforeground = #eeeeee\n",
            )
            .unwrap();
            if let Some(text) = text {
                let name = if dir == &paths.dir {
                    "config.toml"
                } else {
                    "config"
                };
                std::fs::write(dir.join(name), text).unwrap();
            }
        }
        (root, paths)
    }

    #[test]
    fn missing_files_give_the_defaults() {
        let (_root, paths) = paths(None, None);
        assert_eq!(Config::load(&paths), Config::default());
    }

    #[test]
    fn ghostty_fills_the_font_and_theme_with_the_last_theme_winning() {
        let ghostty = "# comment\nfont-size = 14\nfont-family = Iosevka\nfont-family = Symbols\ntheme = nope\ntheme = dusk\n";
        let (_root, paths) = paths(None, Some(ghostty));
        let config = Config::load(&paths);
        assert_eq!(
            config.font,
            FontConfig {
                family: "Iosevka".to_owned(),
                size: 14.0,
                symbols_family: None,
                fallback_families: vec!["Symbols".to_owned()],
                features: Vec::new(),
                shaping_breaks_at_cursor: true,
                adjust_icon_height: None,
            }
        );
        assert_eq!(
            config.theme.background,
            Rgb {
                r: 0x10,
                g: 0x10,
                b: 0x10
            }
        );
        assert_eq!(
            config.theme.palette,
            [(
                1,
                Rgb {
                    r: 0xff,
                    g: 0,
                    b: 0
                }
            )]
        );
    }

    #[test]
    fn shell_integration_is_on_unless_config_toml_or_ghostty_says_none() {
        let enabled = |config, ghostty| Config::load(&paths(config, ghostty).1).shell_integration;
        assert!(enabled(None, None));
        assert!(!enabled(None, Some("shell-integration = none")));
        assert!(enabled(None, Some("shell-integration = detect")));
        assert!(!enabled(Some("shell-integration = \"none\""), None));
        assert!(enabled(
            Some("shell-integration = \"detect\""),
            Some("shell-integration = none")
        ));
    }

    #[test]
    fn adjust_icon_height_comes_from_config_toml_then_ghostty() {
        let adjust = |config: Option<&str>, ghostty: Option<&str>| {
            let (_root, paths) = paths(config, ghostty);
            Config::load(&paths).font.adjust_icon_height
        };
        assert_eq!(adjust(None, None), None);
        assert_eq!(
            adjust(None, Some("adjust-icon-height = -10%\n")),
            Some(Modifier::Percent(0.9))
        );
        assert_eq!(
            adjust(
                Some("[font]\nadjust-icon-height = \"3\"\n"),
                Some("adjust-icon-height = 20%\n")
            ),
            Some(Modifier::Absolute(3))
        );
        assert_eq!(
            adjust(Some("[font]\nadjust-icon-height = \"big\"\n"), None),
            None
        );
    }

    #[test]
    fn clipboard_access_defaults_to_asking_for_reads_and_allowing_writes() {
        let (_root, paths) = paths(None, None);
        let config = Config::load(&paths);
        assert_eq!(config.clipboard_read, ClipboardAccess::Ask);
        assert_eq!(config.clipboard_write, ClipboardAccess::Allow);
    }

    #[test]
    fn clipboard_access_comes_from_config_toml_then_ghostty() {
        let (_root, paths) = paths(
            Some("clipboard-read = \"allow\"\n"),
            Some("clipboard-read = deny\nclipboard-write = ask\n"),
        );
        let config = Config::load(&paths);
        assert_eq!(config.clipboard_read, ClipboardAccess::Allow);
        assert_eq!(config.clipboard_write, ClipboardAccess::Ask);
    }

    #[test]
    fn an_unknown_ghostty_clipboard_value_falls_back_to_the_default() {
        let (_root, paths) = paths(None, Some("clipboard-read = sometimes\n"));
        assert_eq!(Config::load(&paths).clipboard_read, ClipboardAccess::Ask);
    }

    #[test]
    fn config_toml_overrides_ghostty_key_by_key() {
        let config = "shell = \"/bin/zsh\"\njump-command = \"j\"\n[font]\nsize = 12\nsymbols-family = \"Icons NF\"\n[[tabs]]\ntitle = \"shell\"\n";
        let (_root, paths) = paths(
            Some(config),
            Some("font-size = 14\nfont-family = Iosevka\n"),
        );
        let config = Config::load(&paths);
        assert_eq!(config.shell, PathBuf::from("/bin/zsh"));
        assert_eq!(config.jump_command.as_deref(), Some("j"));
        assert_eq!(
            config.font,
            FontConfig {
                family: "Iosevka".to_owned(),
                size: 12.0,
                symbols_family: Some("Icons NF".to_owned()),
                fallback_families: Vec::new(),
                features: Vec::new(),
                shaping_breaks_at_cursor: true,
                adjust_icon_height: None,
            }
        );
        assert_eq!(
            config.tabs,
            [TabSpec {
                title: "shell".to_owned(),
                command: None
            }]
        );
    }

    #[test]
    fn ghostty_font_features_fill_in_and_config_toml_replaces_them() {
        let ghostty = "font-feature = -calt\nfont-feature = ss01\n";
        let features = |config: Option<&str>| {
            let (_root, paths) = paths(config, Some(ghostty));
            Config::load(&paths).font.features
        };
        assert_eq!(features(None), ["-calt", "ss01"]);
        assert_eq!(
            features(Some("[font]\nfeatures = [\"+liga\"]\n")),
            ["+liga"]
        );
        assert!(features(Some("[font]\nfeatures = []\n")).is_empty());
    }

    #[test]
    fn auto_secure_input_reads_ghostty_and_config_toml_replaces_it() {
        let auto = |config: Option<&str>, ghostty: Option<&str>| {
            let (_root, paths) = paths(config, ghostty);
            Config::load(&paths).auto_secure_input
        };
        assert!(auto(None, None));
        assert!(!auto(None, Some("macos-auto-secure-input = false\n")));
        assert!(auto(
            Some("auto-secure-input = true\n"),
            Some("macos-auto-secure-input = false\n")
        ));
        assert!(!auto(Some("auto-secure-input = false\n"), None));
    }

    #[test]
    fn shaping_break_reads_ghostty_and_config_toml_replaces_it() {
        let breaks = |config: Option<&str>, ghostty: Option<&str>| {
            let (_root, paths) = paths(config, ghostty);
            Config::load(&paths).font.shaping_breaks_at_cursor
        };
        assert!(breaks(None, None));
        assert!(!breaks(None, Some("font-shaping-break = no-cursor\n")));
        assert!(breaks(
            None,
            Some("font-shaping-break = no-cursor,cursor\n")
        ));
        assert!(!breaks(
            None,
            Some("font-shaping-break = cursor\nfont-shaping-break = no-cursor\n")
        ));
        let off = "[font]\nshaping-break = [\"no-cursor\"]\n";
        assert!(!breaks(Some(off), None));
        assert!(!breaks(Some(off), Some("font-shaping-break = cursor\n")));
        assert!(breaks(
            Some("[font]\nshaping-break = []\n"),
            Some("font-shaping-break = no-cursor\n")
        ));
    }

    #[test]
    fn a_light_and_dark_theme_picks_the_dark_one() {
        let (_root, paths) = paths(Some("theme = \"light:nope,dark:dusk\"\n"), None);
        assert_eq!(
            Config::load(&paths).theme.foreground,
            Rgb {
                r: 0xee,
                g: 0xee,
                b: 0xee
            }
        );
    }

    #[test]
    fn a_broken_config_falls_back_to_the_defaults() {
        let (_root, paths) = paths(Some("shel = 1"), None);
        assert_eq!(Config::load(&paths), Config::default());
    }
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    fn dir_paths() -> (tempfile::TempDir, Paths) {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            dir: root.path().join("agentd"),
            ghostty_dir: root.path().join("ghostty"),
        };
        (root, paths)
    }

    fn run(paths: &Paths, args: &[&str]) -> Result<String> {
        let mut out = Vec::new();
        command(
            &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
            paths,
            &mut out,
        )?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn read(paths: &Paths, name: &str) -> String {
        std::fs::read_to_string(paths.dir.join(name)).unwrap()
    }

    #[test]
    fn committed_schema_is_up_to_date() {
        let committed = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/schema/config.schema.json"
        ))
        .unwrap_or_default();
        assert!(
            committed == schema(),
            "schema/config.schema.json is out of date; run `cargo run -- config schema > schema/config.schema.json`"
        );
    }

    #[test]
    fn schema_denies_unknown_keys_and_uses_renamed_keys() {
        let schema: serde_json::Value = serde_json::from_str(&schema()).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["properties"]["jump-command"].is_object());
        let font = &schema["$defs"]["FontFile"];
        assert_eq!(font["additionalProperties"], false);
        assert!(font["properties"]["symbols-family"].is_object());
    }

    #[test]
    fn init_writes_the_config_with_the_schema_line_and_the_schema_file() {
        let (_root, paths) = dir_paths();
        run(&paths, &["init"]).unwrap();
        assert!(read(&paths, "config.toml").starts_with("#:schema ./config.schema.json\n"));
        assert_eq!(read(&paths, "config.schema.json"), schema());
    }

    #[test]
    fn init_refuses_to_overwrite_a_config() {
        let (_root, paths) = dir_paths();
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(paths.dir.join("config.toml"), "mine").unwrap();
        assert!(run(&paths, &["init"]).is_err());
        assert_eq!(read(&paths, "config.toml"), "mine");
    }

    #[test]
    fn init_output_loads_as_the_defaults() {
        let (_root, paths) = dir_paths();
        run(&paths, &["init"]).unwrap();
        toml::from_str::<File>(&read(&paths, "config.toml")).unwrap();
        assert_eq!(Config::load(&paths), Config::default());
    }

    #[test]
    fn init_output_keeps_the_ghostty_fallbacks() {
        let (_root, paths) = dir_paths();
        std::fs::create_dir_all(&paths.ghostty_dir).unwrap();
        std::fs::write(
            paths.ghostty_dir.join("config"),
            "font-family = Iosevka\nfont-size = 14\n",
        )
        .unwrap();
        let without_config = Config::load(&paths);
        run(&paths, &["init"]).unwrap();
        assert_eq!(Config::load(&paths), without_config);
        assert_eq!(without_config.font.family, "Iosevka");
    }

    #[test]
    fn migrate_adds_the_line_and_keeps_comments_and_order() {
        let (_root, paths) = dir_paths();
        std::fs::create_dir_all(&paths.dir).unwrap();
        let original = "# mine\nshell = \"/bin/zsh\"  # sh\n\n[font]\nsize = 12\n";
        std::fs::write(paths.dir.join("config.toml"), original).unwrap();
        run(&paths, &["migrate"]).unwrap();
        assert_eq!(
            read(&paths, "config.toml"),
            format!("#:schema ./config.schema.json\n{original}")
        );
        assert!(paths.dir.join("config.schema.json").is_file());
    }

    #[test]
    fn migrate_replaces_a_stale_schema_line_and_is_idempotent() {
        let (_root, paths) = dir_paths();
        std::fs::create_dir_all(&paths.dir).unwrap();
        std::fs::write(
            paths.dir.join("config.toml"),
            "#:schema https://example.com/old.json\nshell = \"/bin/zsh\"\n",
        )
        .unwrap();
        run(&paths, &["migrate"]).unwrap();
        let once = read(&paths, "config.toml");
        assert_eq!(
            once,
            "#:schema ./config.schema.json\nshell = \"/bin/zsh\"\n"
        );
        run(&paths, &["migrate"]).unwrap();
        assert_eq!(read(&paths, "config.toml"), once);
    }

    #[test]
    fn schema_prints_the_schema() {
        let (_root, paths) = dir_paths();
        assert_eq!(run(&paths, &["schema"]).unwrap(), schema());
    }
}
