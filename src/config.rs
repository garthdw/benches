use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use libghostty_vt::style::RgbColor;
use serde::Deserialize;

use crate::session::{DEFAULT_BG, DEFAULT_FG, LogErr};

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
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    pub size: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct TabSpec {
    pub title: String,
    /// `None` runs the launch shell.
    pub command: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub foreground: RgbColor,
    pub background: RgbColor,
    pub cursor: Option<RgbColor>,
    pub selection_background: Option<RgbColor>,
    pub selection_foreground: Option<RgbColor>,
    /// Overrides of the built-in 256-colour palette.
    pub palette: Vec<(u8, RgbColor)>,
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
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    shell: Option<PathBuf>,
    theme: Option<String>,
    #[serde(default)]
    font: FontFile,
    tabs: Option<Vec<TabSpec>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FontFile {
    family: Option<String>,
    size: Option<f64>,
}

/// Where to look for the files the config names.
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
        let defaults = Config::default();
        let ghostty_size = ghostty_value("font-size")
            .and_then(|size| size.parse::<f64>().log_err("ghostty font-size"));
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
                    .or_else(|| {
                        ghostty_values(&ghostty, "font-family")
                            .first()
                            .map(|family| (*family).to_owned())
                    })
                    .unwrap_or(defaults.font.family),
                size: file
                    .font
                    .size
                    .or(ghostty_size)
                    .unwrap_or(defaults.font.size),
            },
            theme,
            tabs: file.tabs.unwrap_or(defaults.tabs),
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
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

fn parse_color(value: &str) -> Result<RgbColor> {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() != 6 {
        bail!("colour {value} isn't #rrggbb");
    }
    let channel =
        |range| u8::from_str_radix(&hex[range], 16).with_context(|| format!("colour {value}"));
    Ok(RgbColor {
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
                size: 14.0
            }
        );
        assert_eq!(
            config.theme.background,
            RgbColor {
                r: 0x10,
                g: 0x10,
                b: 0x10
            }
        );
        assert_eq!(
            config.theme.palette,
            [(
                1,
                RgbColor {
                    r: 0xff,
                    g: 0,
                    b: 0
                }
            )]
        );
    }

    #[test]
    fn config_toml_overrides_ghostty_key_by_key() {
        let config = "shell = \"/bin/zsh\"\n[font]\nsize = 12\n[[tabs]]\ntitle = \"shell\"\n";
        let (_root, paths) = paths(
            Some(config),
            Some("font-size = 14\nfont-family = Iosevka\n"),
        );
        let config = Config::load(&paths);
        assert_eq!(config.shell, PathBuf::from("/bin/zsh"));
        assert_eq!(
            config.font,
            FontConfig {
                family: "Iosevka".to_owned(),
                size: 12.0
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
    fn a_light_and_dark_theme_picks_the_dark_one() {
        let (_root, paths) = paths(Some("theme = \"light:nope,dark:dusk\"\n"), None);
        assert_eq!(
            Config::load(&paths).theme.foreground,
            RgbColor {
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
