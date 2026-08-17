use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs::File,
    io::Read,
    num::NonZeroU8,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use crate::diagnostics::{ClassifiedError, ErrorCategory, escape_diagnostic};

pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct EffectiveConfig {
    pub terminal: TerminalConfig,
    pub font: FontConfig,
    pub colors: ColorsConfig,
    pub window: WindowConfig,
    pub scrolling: ScrollingConfig,
    pub scrollbar: ScrollbarConfig,
    pub cursor: CursorConfig,
    pub behavior: BehaviorConfig,
    pub unicode: UnicodeConfig,
    pub tabs: TabsConfig,
    pub bell: BellConfig,
    pub keybindings: Vec<KeyBinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalConfig {
    pub identity: crate::terminfo::TerminalIdentity,
}

pub const MAX_TABS: u8 = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TabsConfig {
    pub max_count: u8,
    pub bar_height: u16,
    pub min_width: u16,
    pub max_width: u16,
    pub show_close_button: bool,
    pub new_tab_cwd: NewTabCwdPolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct BellConfig {
    pub enabled: bool,
    pub visual: bool,
    pub visual_duration: Duration,
    pub audible: bool,
    pub audible_when_unfocused: bool,
    pub attention: bool,
    pub desktop_notifications: bool,
    pub notification_cooldown: Duration,
    pub notification_burst_per_minute: NonZeroU8,
}

impl Default for BellConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            visual: true,
            visual_duration: Duration::from_millis(120),
            audible: false,
            audible_when_unfocused: true,
            attention: true,
            desktop_notifications: true,
            notification_cooldown: Duration::from_secs(10),
            notification_burst_per_minute: NonZeroU8::new(6).expect("non-zero constant"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NewTabCwdPolicy {
    Inherit,
    Fixed(PathBuf),
    Home,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub family: String,
    pub size: f64,
    pub ligatures: bool,
    pub hinting: HintingPreference,
    pub antialiasing: AntialiasPreference,
    pub line_spacing: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HintingPreference {
    None,
    Slight,
    Full,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AntialiasPreference {
    Grayscale,
    System,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Color(pub u32);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ColorsConfig {
    pub foreground: Color,
    pub background: Color,
    pub cursor: Color,
    pub selection_foreground: Color,
    pub selection_background: Color,
    pub ansi: [Color; 16],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowConfig {
    pub padding_x: u16,
    pub padding_y: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScrollingConfig {
    pub history_lines: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollbarMode {
    Auto,
    Always,
    Hidden,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollbarConfig {
    pub mode: ScrollbarMode,
    pub width: f64,
    pub hit_width: f64,
    pub min_thumb_size: f64,
    pub thumb: Color,
    pub thumb_hover: Color,
    pub track: Color,
}

pub const DEFAULT_ANSI_PALETTE: [Color; 16] = [
    Color(0x2e34_36ff),
    Color(0xcc66_66ff),
    Color(0x6fa6_6fff),
    Color(0xc8a8_5fff),
    Color(0x5f87_afff),
    Color(0xa27a_a8ff),
    Color(0x5f9e_a0ff),
    Color(0xd3d7_cfff),
    Color(0x6c73_75ff),
    Color(0xe07a_7aff),
    Color(0x87bd_87ff),
    Color(0xd8bd_73ff),
    Color(0x7aa2_c8ff),
    Color(0xb58a_bbff),
    Color(0x72b2_b4ff),
    Color(0xeeee_ecff),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorStyle {
    Block,
    Beam,
    Underline,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorConfig {
    pub style: CursorStyle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BehaviorConfig {
    pub hold_after_exit: bool,
    pub confirm_multiline_paste: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnicodeConfig {
    pub bidi: bool,
    pub color_glyphs: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    pub chord: crate::input::shortcut::KeyChord,
    pub action: Action,
    pub origin: crate::input::shortcut::BindingOrigin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Modifier {
    Control,
    Shift,
    Alt,
    Super,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    CopyClipboard,
    PasteClipboard,
    PastePrimary,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    ScrollPageUp,
    ScrollPageDown,
    NewTab,
    CloseTab,
    PreviousTab,
    NextTab,
    ActivateTab(u8),
    ToggleBellMute,
}

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            terminal: TerminalConfig {
                identity: crate::terminfo::TerminalIdentity::Leyline,
            },
            font: FontConfig {
                family: "monospace".into(),
                size: 13.0,
                ligatures: false,
                hinting: HintingPreference::Slight,
                antialiasing: AntialiasPreference::Grayscale,
                line_spacing: 1.0,
            },
            colors: ColorsConfig {
                foreground: Color(0xd8dc_d8ff),
                background: Color(0x2e34_36f2),
                cursor: Color(0xd8dc_d8ff),
                selection_foreground: Color(0xf4f6_f4ff),
                selection_background: Color(0x5865_6dcc),
                ansi: DEFAULT_ANSI_PALETTE,
            },
            window: WindowConfig {
                padding_x: 0,
                padding_y: 5,
            },
            scrolling: ScrollingConfig {
                history_lines: 10_000,
            },
            scrollbar: ScrollbarConfig {
                mode: ScrollbarMode::Auto,
                width: 4.0,
                hit_width: 12.0,
                min_thumb_size: 24.0,
                thumb: Color(0x9aa2_a680),
                thumb_hover: Color(0xc1c7_caff),
                track: Color(0x0000_0000),
            },
            cursor: CursorConfig {
                style: CursorStyle::Block,
            },
            behavior: BehaviorConfig {
                hold_after_exit: false,
                confirm_multiline_paste: true,
            },
            unicode: UnicodeConfig {
                bidi: false,
                color_glyphs: true,
            },
            tabs: TabsConfig {
                max_count: MAX_TABS,
                bar_height: 32,
                min_width: 80,
                max_width: 240,
                show_close_button: true,
                new_tab_cwd: NewTabCwdPolicy::Inherit,
            },
            bell: BellConfig::default(),
            keybindings: default_keybindings(),
        }
    }
}

fn default_keybindings() -> Vec<KeyBinding> {
    use crate::input::shortcut::{BindingOrigin, KeyChord, LogicalKeyPattern};
    use Action::{
        CloseTab, CopyClipboard, DecreaseFontSize, IncreaseFontSize, NewTab, NextTab,
        PasteClipboard, PastePrimary, PreviousTab, ResetFontSize, ScrollPageDown, ScrollPageUp,
    };
    use leyline_gfx::ModifierMask;
    [
        (
            LogicalKeyPattern::Character('c'),
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            CopyClipboard,
        ),
        (
            LogicalKeyPattern::Character('v'),
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            PasteClipboard,
        ),
        (LogicalKeyPattern::Insert, ModifierMask::SHIFT, PastePrimary),
        (
            LogicalKeyPattern::Character('+'),
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            IncreaseFontSize,
        ),
        (
            LogicalKeyPattern::Character('='),
            ModifierMask::CONTROL,
            IncreaseFontSize,
        ),
        (
            LogicalKeyPattern::Character('-'),
            ModifierMask::CONTROL,
            DecreaseFontSize,
        ),
        (
            LogicalKeyPattern::Character('0'),
            ModifierMask::CONTROL,
            ResetFontSize,
        ),
        (LogicalKeyPattern::PageUp, ModifierMask::SHIFT, ScrollPageUp),
        (
            LogicalKeyPattern::PageDown,
            ModifierMask::SHIFT,
            ScrollPageDown,
        ),
        (
            LogicalKeyPattern::Character('N'),
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            NewTab,
        ),
        (
            LogicalKeyPattern::Character('W'),
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            CloseTab,
        ),
        (
            LogicalKeyPattern::ArrowLeft,
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            PreviousTab,
        ),
        (
            LogicalKeyPattern::ArrowRight,
            modifier_mask(&[Modifier::Control, Modifier::Shift]),
            NextTab,
        ),
    ]
    .into_iter()
    .map(|(key, modifiers, action)| KeyBinding {
        chord: KeyChord { key, modifiers },
        action,
        origin: BindingOrigin::Default,
    })
    .collect::<Vec<_>>()
    .into_iter()
    .chain((1_u8..=9).map(|number| KeyBinding {
        chord: KeyChord {
            key: LogicalKeyPattern::Character(char::from(b'0' + number)),
            modifiers: modifier_mask(&[Modifier::Control, Modifier::Shift]),
        },
        action: Action::ActivateTab(number),
        origin: BindingOrigin::Default,
    }))
    .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigOrigin {
    Default,
    File(PathBuf),
}

#[derive(Clone, Debug)]
pub struct LoadedConfig {
    pub effective: EffectiveConfig,
    pub warnings: Vec<ConfigWarning>,
    pub source: ConfigOrigin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigWarning {
    pub message: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigLocation {
    Missing(PathBuf),
    File(PathBuf),
}

pub trait ConfigEnvironment {
    fn xdg_config_home(&self) -> Option<OsString>;
    fn home(&self) -> Option<OsString>;
}

pub trait ConfigSource {
    /// Resolves the configured XDG location.
    ///
    /// # Errors
    /// Returns [`ConfigError`] when no absolute configuration base is available.
    fn locate(&self) -> Result<ConfigLocation, ConfigError>;
    /// Loads and validates a resolved configuration.
    ///
    /// # Errors
    /// Returns [`ConfigError`] for I/O, encoding, TOML, or semantic failures.
    fn load(&self, location: &ConfigLocation) -> Result<LoadedConfig, ConfigError>;
}

pub struct FileConfigSource<E> {
    environment: E,
}

impl<E> FileConfigSource<E> {
    pub const fn new(environment: E) -> Self {
        Self { environment }
    }
}

impl<E: ConfigEnvironment> ConfigSource for FileConfigSource<E> {
    fn locate(&self) -> Result<ConfigLocation, ConfigError> {
        let base = absolute_env_path(self.environment.xdg_config_home())
            .or_else(|| absolute_env_path(self.environment.home()).map(|home| home.join(".config")))
            .ok_or(ConfigError::PathUnavailable)?;
        let path = base.join("leyline/config.toml");
        match path.try_exists() {
            Ok(true) => Ok(ConfigLocation::File(path)),
            Ok(false) => Ok(ConfigLocation::Missing(path)),
            Err(source) => Err(ConfigError::Read { path, source }),
        }
    }

    fn load(&self, location: &ConfigLocation) -> Result<LoadedConfig, ConfigError> {
        let ConfigLocation::File(path) = location else {
            return Ok(LoadedConfig {
                effective: EffectiveConfig::default(),
                warnings: Vec::new(),
                source: ConfigOrigin::Default,
            });
        };
        let text = read_config(path)?;
        let raw = toml::from_str::<RawConfig>(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        let (effective, warnings) = raw.into_effective(path, &text)?;
        Ok(LoadedConfig {
            effective,
            warnings,
            source: ConfigOrigin::File(path.clone()),
        })
    }
}

fn absolute_env_path(value: Option<OsString>) -> Option<PathBuf> {
    let path = PathBuf::from(value?);
    (!path.as_os_str().is_empty() && path.is_absolute()).then_some(path)
}

fn read_config(path: &Path) -> Result<String, ConfigError> {
    let file = File::open(path).map_err(|source| ConfigError::Read {
        path: path.into(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| ConfigError::Read {
        path: path.into(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(ConfigError::NotRegularFile(path.into()));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len().try_into().unwrap_or(0));
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Read {
            path: path.into(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge(path.into()));
    }
    String::from_utf8(bytes).map_err(|source| ConfigError::Utf8 {
        path: path.into(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no absolute configuration directory is available; set XDG_CONFIG_HOME to an absolute path"
    )]
    PathUnavailable,
    #[error("cannot read configuration file {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("configuration path {} does not resolve to a regular file", .0.display())]
    NotRegularFile(PathBuf),
    #[error("configuration file {} exceeds the 1 MiB limit", .0.display())]
    TooLarge(PathBuf),
    #[error("configuration file {} is not valid UTF-8: {source}", path.display())]
    Utf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
    #[error("invalid TOML in {}: {source}", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration field {field} in {}: {reason}", path.display())]
    Semantic {
        path: PathBuf,
        field: String,
        reason: String,
    },
}

impl ClassifiedError for ConfigError {
    fn category(&self) -> ErrorCategory {
        if matches!(self, Self::PathUnavailable) {
            ErrorCategory::Environment
        } else {
            ErrorCategory::Configuration
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    terminal: RawTerminal,
    font: RawFont,
    colors: RawColors,
    window: RawWindow,
    scrolling: RawScrolling,
    scrollbar: RawScrollbar,
    cursor: RawCursor,
    behavior: RawBehavior,
    unicode: RawUnicode,
    tabs: RawTabs,
    bell: RawBell,
    keybindings: Option<Vec<RawKeyBinding>>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBell {
    enabled: Option<bool>,
    visual: Option<bool>,
    visual_duration_ms: Option<i64>,
    audible: Option<bool>,
    audible_when_unfocused: Option<bool>,
    attention: Option<bool>,
    desktop_notifications: Option<bool>,
    notification_cooldown_seconds: Option<i64>,
    notification_burst_per_minute: Option<i64>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTerminal {
    identity: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawTabs {
    max_count: Option<i64>,
    bar_height: Option<i64>,
    min_width: Option<i64>,
    max_width: Option<i64>,
    show_close_button: Option<bool>,
    new_tab_cwd: Option<String>,
    new_tab_fixed_cwd: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFont {
    family: Option<String>,
    size: Option<f64>,
    ligatures: Option<bool>,
    hinting: Option<String>,
    antialiasing: Option<String>,
    line_spacing: Option<f64>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawColors {
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    selection_foreground: Option<String>,
    selection_background: Option<String>,
    ansi: Option<Vec<String>>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWindow {
    padding_x: Option<i64>,
    padding_y: Option<i64>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawScrolling {
    history_lines: Option<i64>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawScrollbar {
    mode: Option<String>,
    width: Option<f64>,
    hit_width: Option<f64>,
    min_thumb_size: Option<f64>,
    thumb: Option<String>,
    thumb_hover: Option<String>,
    track: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawCursor {
    style: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBehavior {
    hold_after_exit: Option<bool>,
    confirm_multiline_paste: Option<bool>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUnicode {
    bidi: Option<bool>,
    color_glyphs: Option<bool>,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}
#[derive(Debug, Deserialize)]
struct RawKeyBinding {
    key: String,
    mods: Vec<String>,
    action: String,
    #[serde(flatten)]
    unknown: BTreeMap<String, toml::Value>,
}

impl RawConfig {
    #[allow(clippy::too_many_lines)]
    fn into_effective(
        self,
        path: &Path,
        source: &str,
    ) -> Result<(EffectiveConfig, Vec<ConfigWarning>), ConfigError> {
        let mut result = EffectiveConfig::default();
        let mut warnings = Vec::new();
        collect_unknown(&mut warnings, source, "", &self.unknown, TOP_FIELDS);
        collect_unknown(
            &mut warnings,
            source,
            "terminal",
            &self.terminal.unknown,
            &["identity"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "font",
            &self.font.unknown,
            &[
                "family",
                "size",
                "ligatures",
                "hinting",
                "antialiasing",
                "line_spacing",
            ],
        );
        collect_unknown(
            &mut warnings,
            source,
            "bell",
            &self.bell.unknown,
            &[
                "enabled",
                "visual",
                "visual_duration_ms",
                "audible",
                "audible_when_unfocused",
                "attention",
                "desktop_notifications",
                "notification_cooldown_seconds",
                "notification_burst_per_minute",
            ],
        );

        if let Some(value) = self.terminal.identity {
            result.terminal.identity = crate::terminfo::TerminalIdentity::parse(&value)
                .ok_or_else(|| ConfigError::Semantic {
                    path: path.into(),
                    field: "terminal.identity".into(),
                    reason: format!(
                        "{} is not one of leyline, xterm-256color",
                        escape_diagnostic(&value)
                    ),
                })?;
        }
        collect_unknown(
            &mut warnings,
            source,
            "colors",
            &self.colors.unknown,
            &[
                "foreground",
                "background",
                "cursor",
                "selection_foreground",
                "selection_background",
                "ansi",
            ],
        );
        collect_unknown(
            &mut warnings,
            source,
            "window",
            &self.window.unknown,
            &["padding_x", "padding_y"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "scrollbar",
            &self.scrollbar.unknown,
            &[
                "mode",
                "width",
                "hit_width",
                "min_thumb_size",
                "thumb",
                "thumb_hover",
                "track",
            ],
        );
        collect_unknown(
            &mut warnings,
            source,
            "scrolling",
            &self.scrolling.unknown,
            &["history_lines"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "cursor",
            &self.cursor.unknown,
            &["style"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "behavior",
            &self.behavior.unknown,
            &["hold_after_exit", "confirm_multiline_paste"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "unicode",
            &self.unicode.unknown,
            &["bidi", "color_glyphs"],
        );
        collect_unknown(
            &mut warnings,
            source,
            "tabs",
            &self.tabs.unknown,
            &[
                "max_count",
                "bar_height",
                "min_width",
                "max_width",
                "show_close_button",
                "new_tab_cwd",
                "new_tab_fixed_cwd",
            ],
        );

        if let Some(family) = self.font.family {
            result.font.family = family;
        }
        if let Some(size) = self.font.size {
            if !size.is_finite() || !(6.0..=72.0).contains(&size) {
                return semantic(path, "font.size", format!("{size:?} is outside 6.0..=72.0"));
            }
            result.font.size = size;
        }
        if let Some(value) = self.font.ligatures {
            result.font.ligatures = value;
        }
        if let Some(value) = self.font.hinting {
            result.font.hinting = match value.as_str() {
                "none" => HintingPreference::None,
                "slight" => HintingPreference::Slight,
                "full" => HintingPreference::Full,
                "system" => HintingPreference::System,
                _ => {
                    return semantic(
                        path,
                        "font.hinting",
                        format!(
                            "{} is not one of none, slight, full, system",
                            escape_diagnostic(&value)
                        ),
                    );
                }
            };
        }
        if let Some(value) = self.font.antialiasing {
            result.font.antialiasing = match value.as_str() {
                "grayscale" => AntialiasPreference::Grayscale,
                "system" => AntialiasPreference::System,
                _ => {
                    return semantic(
                        path,
                        "font.antialiasing",
                        format!(
                            "{} is not one of grayscale, system",
                            escape_diagnostic(&value)
                        ),
                    );
                }
            };
        }
        if let Some(value) = self.font.line_spacing {
            validate_float(path, "font.line_spacing", value, 0.0, 8.0)?;
            result.font.line_spacing = value;
        }
        if let Some(value) = self.unicode.bidi {
            result.unicode.bidi = value;
        }
        if let Some(value) = self.unicode.color_glyphs {
            result.unicode.color_glyphs = value;
        }
        set_color(
            path,
            "colors.foreground",
            self.colors.foreground,
            &mut result.colors.foreground,
        )?;
        if let Some(values) = self.colors.ansi {
            if values.len() != 16 {
                return semantic(
                    path,
                    "colors.ansi",
                    format!("must contain exactly 16 colors, got {}", values.len()),
                );
            }
            for (index, raw) in values.into_iter().enumerate() {
                let color = parse_opaque_color(&raw).ok_or_else(|| ConfigError::Semantic {
                    path: path.into(),
                    field: format!("colors.ansi[{index}]"),
                    reason: format!("{} must be #RRGGBB", escape_diagnostic(&raw)),
                })?;
                result.colors.ansi[index] = color;
            }
        }
        set_color(
            path,
            "colors.background",
            self.colors.background,
            &mut result.colors.background,
        )?;
        set_color(
            path,
            "colors.cursor",
            self.colors.cursor,
            &mut result.colors.cursor,
        )?;
        set_color(
            path,
            "colors.selection_foreground",
            self.colors.selection_foreground,
            &mut result.colors.selection_foreground,
        )?;
        set_color(
            path,
            "colors.selection_background",
            self.colors.selection_background,
            &mut result.colors.selection_background,
        )?;
        set_bounded(
            path,
            "window.padding_x",
            self.window.padding_x,
            0,
            256,
            &mut result.window.padding_x,
        )?;
        if let Some(value) = self.scrollbar.mode {
            result.scrollbar.mode = match value.as_str() {
                "auto" => ScrollbarMode::Auto,
                "always" => ScrollbarMode::Always,
                "hidden" => ScrollbarMode::Hidden,
                _ => {
                    return semantic(
                        path,
                        "scrollbar.mode",
                        format!(
                            "{} is not one of auto, always, hidden",
                            escape_diagnostic(&value)
                        ),
                    );
                }
            };
        }
        set_float(
            path,
            "scrollbar.width",
            self.scrollbar.width,
            2.0,
            32.0,
            &mut result.scrollbar.width,
        )?;
        set_float(
            path,
            "scrollbar.hit_width",
            self.scrollbar.hit_width,
            2.0,
            32.0,
            &mut result.scrollbar.hit_width,
        )?;
        set_float(
            path,
            "scrollbar.min_thumb_size",
            self.scrollbar.min_thumb_size,
            2.0,
            32.0,
            &mut result.scrollbar.min_thumb_size,
        )?;
        if result.scrollbar.hit_width < result.scrollbar.width {
            return semantic(
                path,
                "scrollbar.hit_width",
                "must be greater than or equal to scrollbar.width".into(),
            );
        }
        set_color(
            path,
            "scrollbar.thumb",
            self.scrollbar.thumb,
            &mut result.scrollbar.thumb,
        )?;
        set_color(
            path,
            "scrollbar.thumb_hover",
            self.scrollbar.thumb_hover,
            &mut result.scrollbar.thumb_hover,
        )?;
        set_color(
            path,
            "scrollbar.track",
            self.scrollbar.track,
            &mut result.scrollbar.track,
        )?;
        set_bounded(
            path,
            "window.padding_y",
            self.window.padding_y,
            0,
            256,
            &mut result.window.padding_y,
        )?;
        set_bounded(
            path,
            "scrolling.history_lines",
            self.scrolling.history_lines,
            0,
            100_000,
            &mut result.scrolling.history_lines,
        )?;
        if let Some(style) = self.cursor.style {
            result.cursor.style = match style.as_str() {
                "block" => CursorStyle::Block,
                "beam" => CursorStyle::Beam,
                "underline" => CursorStyle::Underline,
                _ => {
                    return semantic(
                        path,
                        "cursor.style",
                        format!(
                            "{} is not one of block, beam, underline",
                            escape_diagnostic(&style)
                        ),
                    );
                }
            };
        }
        if let Some(value) = self.behavior.hold_after_exit {
            result.behavior.hold_after_exit = value;
        }
        if let Some(value) = self.behavior.confirm_multiline_paste {
            result.behavior.confirm_multiline_paste = value;
        }
        if let Some(value) = self.bell.enabled {
            result.bell.enabled = value;
        }
        if let Some(value) = self.bell.visual {
            result.bell.visual = value;
        }
        if let Some(value) = self.bell.audible {
            result.bell.audible = value;
        }
        if let Some(value) = self.bell.audible_when_unfocused {
            result.bell.audible_when_unfocused = value;
        }
        if let Some(value) = self.bell.attention {
            result.bell.attention = value;
        }
        if let Some(value) = self.bell.desktop_notifications {
            result.bell.desktop_notifications = value;
        }
        if let Some(value) = self.bell.visual_duration_ms {
            if !(40..=1000).contains(&value) {
                return semantic(
                    path,
                    "bell.visual_duration_ms",
                    format!("{value} is outside 40..=1000"),
                );
            }
            result.bell.visual_duration = Duration::from_millis(
                u64::try_from(value).expect("validated non-negative duration"),
            );
        }
        if let Some(value) = self.bell.notification_cooldown_seconds {
            if !(1..=3600).contains(&value) {
                return semantic(
                    path,
                    "bell.notification_cooldown_seconds",
                    format!("{value} is outside 1..=3600"),
                );
            }
            result.bell.notification_cooldown =
                Duration::from_secs(u64::try_from(value).expect("validated non-negative duration"));
        }
        if let Some(value) = self.bell.notification_burst_per_minute {
            if !(1..=30).contains(&value) {
                return semantic(
                    path,
                    "bell.notification_burst_per_minute",
                    format!("{value} is outside 1..=30"),
                );
            }
            result.bell.notification_burst_per_minute =
                NonZeroU8::new(u8::try_from(value).expect("validated u8 range"))
                    .expect("validated non-zero");
        }
        set_bounded(
            path,
            "tabs.max_count",
            self.tabs.max_count,
            1,
            i64::from(MAX_TABS),
            &mut result.tabs.max_count,
        )?;
        set_bounded(
            path,
            "tabs.bar_height",
            self.tabs.bar_height,
            24,
            64,
            &mut result.tabs.bar_height,
        )?;
        set_bounded(
            path,
            "tabs.min_width",
            self.tabs.min_width,
            48,
            240,
            &mut result.tabs.min_width,
        )?;
        set_bounded(
            path,
            "tabs.max_width",
            self.tabs.max_width,
            48,
            400,
            &mut result.tabs.max_width,
        )?;
        if result.tabs.min_width > result.tabs.max_width {
            return semantic(
                path,
                "tabs.min_width",
                "must be less than or equal to tabs.max_width".into(),
            );
        }
        if let Some(value) = self.tabs.show_close_button {
            result.tabs.show_close_button = value;
        }
        let cwd_policy = self.tabs.new_tab_cwd.as_deref().unwrap_or("inherit");
        result.tabs.new_tab_cwd =
            match cwd_policy {
                "inherit" => NewTabCwdPolicy::Inherit,
                "home" => NewTabCwdPolicy::Home,
                "fixed" => {
                    let value = self.tabs.new_tab_fixed_cwd.as_ref().ok_or_else(|| {
                        ConfigError::Semantic {
                            path: path.into(),
                            field: "tabs.new_tab_fixed_cwd".into(),
                            reason: "is required when tabs.new_tab_cwd is fixed".into(),
                        }
                    })?;
                    let fixed = PathBuf::from(value);
                    if !fixed.is_absolute() || fixed.as_os_str().as_bytes().contains(&0) {
                        return semantic(
                            path,
                            "tabs.new_tab_fixed_cwd",
                            "must be an absolute path without NUL".into(),
                        );
                    }
                    NewTabCwdPolicy::Fixed(fixed)
                }
                value => {
                    return semantic(
                        path,
                        "tabs.new_tab_cwd",
                        format!(
                            "{} is not one of inherit, fixed, home",
                            escape_diagnostic(value)
                        ),
                    );
                }
            };
        if cwd_policy != "fixed" && self.tabs.new_tab_fixed_cwd.is_some() {
            warnings.push(ConfigWarning {
                message: "tabs.new_tab_fixed_cwd is ignored unless tabs.new_tab_cwd is fixed"
                    .into(),
            });
        }

        if let Some(bindings) = self.keybindings {
            let mut positions = result
                .keybindings
                .iter()
                .enumerate()
                .map(|(index, binding)| (chord(binding), index))
                .collect::<HashMap<_, _>>();
            for (source_index, raw) in bindings.into_iter().enumerate() {
                collect_unknown(
                    &mut warnings,
                    source,
                    &format!("keybindings[{source_index}]"),
                    &raw.unknown,
                    &["key", "mods", "action"],
                );
                let binding = parse_binding(path, source_index, raw)?;
                let chord = chord(&binding);
                if let Some(index) = positions.get(&chord).copied() {
                    result.keybindings[index] = binding;
                    warnings.push(ConfigWarning {
                        message: format!("duplicate keybinding {chord:?}; the later binding wins"),
                    });
                } else {
                    positions.insert(chord, result.keybindings.len());
                    result.keybindings.push(binding);
                }
            }
        }
        Ok((result, warnings))
    }
}

const TOP_FIELDS: &[&str] = &[
    "terminal",
    "font",
    "colors",
    "window",
    "scrolling",
    "scrollbar",
    "cursor",
    "behavior",
    "unicode",
    "tabs",
    "bell",
    "keybindings",
];

fn collect_unknown(
    warnings: &mut Vec<ConfigWarning>,
    source: &str,
    prefix: &str,
    fields: &BTreeMap<String, toml::Value>,
    candidates: &[&str],
) {
    for key in fields.keys() {
        let field = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let line = source
            .lines()
            .position(|line| {
                line.trim_start().starts_with(&format!("{key} "))
                    || line.trim_start().starts_with(&format!("{key}="))
            })
            .map_or(String::new(), |index| format!(" at line {}", index + 1));
        let suggestion = unique_suggestion(key, candidates)
            .map_or(String::new(), |name| format!("; did you mean {name}?"));
        warnings.push(ConfigWarning {
            message: format!("unknown configuration field {field}{line}{suggestion}"),
        });
    }
}

fn unique_suggestion<'a>(input: &str, candidates: &'a [&str]) -> Option<&'a str> {
    let mut matches = candidates
        .iter()
        .copied()
        .filter(|candidate| edit_distance(input, candidate) <= 2);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn edit_distance(left: &str, right: &str) -> usize {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (i, a) in left.bytes().enumerate() {
        let mut current = vec![i + 1];
        for (j, b) in right.bytes().enumerate() {
            current.push(
                (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(a != b)),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

fn set_color(
    path: &Path,
    field: &str,
    raw: Option<String>,
    target: &mut Color,
) -> Result<(), ConfigError> {
    if let Some(raw) = raw {
        *target = parse_color(&raw).ok_or_else(|| ConfigError::Semantic {
            path: path.into(),
            field: field.into(),
            reason: format!("{} must be #RRGGBB or #RRGGBBAA", escape_diagnostic(&raw)),
        })?;
    }
    Ok(())
}

fn parse_color(raw: &str) -> Option<Color> {
    let hex = raw.strip_prefix('#')?;
    if !matches!(hex.len(), 6 | 8) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut value = u32::from_str_radix(hex, 16).ok()?;
    if hex.len() == 6 {
        value = (value << 8) | 0xff;
    }
    Some(Color(value))
}

fn parse_opaque_color(raw: &str) -> Option<Color> {
    (raw.len() == 7).then(|| parse_color(raw)).flatten()
}

fn validate_float(
    path: &Path,
    field: &str,
    value: f64,
    min: f64,
    max: f64,
) -> Result<(), ConfigError> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return semantic(path, field, format!("{value:?} is outside {min}..={max}"));
    }
    Ok(())
}

fn set_float(
    path: &Path,
    field: &str,
    raw: Option<f64>,
    min: f64,
    max: f64,
    target: &mut f64,
) -> Result<(), ConfigError> {
    if let Some(value) = raw {
        validate_float(path, field, value, min, max)?;
        *target = value;
    }
    Ok(())
}

fn set_bounded<T>(
    path: &Path,
    field: &str,
    raw: Option<i64>,
    min: i64,
    max: i64,
    target: &mut T,
) -> Result<(), ConfigError>
where
    T: TryFrom<i64>,
{
    if let Some(raw) = raw {
        if !(min..=max).contains(&raw) {
            return semantic(path, field, format!("{raw} is outside {min}..={max}"));
        }
        *target = T::try_from(raw).map_err(|_| ConfigError::Semantic {
            path: path.into(),
            field: field.into(),
            reason: format!("{raw} cannot be represented"),
        })?;
    }
    Ok(())
}

fn parse_binding(path: &Path, index: usize, raw: RawKeyBinding) -> Result<KeyBinding, ConfigError> {
    let key = crate::input::shortcut::parse_key(&raw.key).map_err(|_| ConfigError::Semantic {
        path: path.into(),
        field: format!("keybindings[{index}].key"),
        reason: format!("unknown logical key {}", escape_diagnostic(&raw.key)),
    })?;
    let mut mods = raw
        .mods
        .into_iter()
        .map(|value| match value.as_str() {
            "Control" => Ok(Modifier::Control),
            "Shift" => Ok(Modifier::Shift),
            "Alt" => Ok(Modifier::Alt),
            "Super" => Ok(Modifier::Super),
            _ => Err(ConfigError::Semantic {
                path: path.into(),
                field: format!("keybindings[{index}].mods"),
                reason: format!("unknown modifier {}", escape_diagnostic(&value)),
            }),
        })
        .collect::<Result<Vec<_>, _>>()?;
    mods.sort_unstable();
    mods.dedup();
    let action = match raw.action.as_str() {
        "CopyClipboard" => Action::CopyClipboard,
        "PasteClipboard" => Action::PasteClipboard,
        "PastePrimary" => Action::PastePrimary,
        "IncreaseFontSize" => Action::IncreaseFontSize,
        "DecreaseFontSize" => Action::DecreaseFontSize,
        "ResetFontSize" => Action::ResetFontSize,
        "ScrollPageUp" => Action::ScrollPageUp,
        "ScrollPageDown" => Action::ScrollPageDown,
        "NewTab" => Action::NewTab,
        "CloseTab" => Action::CloseTab,
        "PreviousTab" => Action::PreviousTab,
        "NextTab" => Action::NextTab,
        "ToggleBellMute" => Action::ToggleBellMute,
        "ActivateTab1" => Action::ActivateTab(1),
        "ActivateTab2" => Action::ActivateTab(2),
        "ActivateTab3" => Action::ActivateTab(3),
        "ActivateTab4" => Action::ActivateTab(4),
        "ActivateTab5" => Action::ActivateTab(5),
        "ActivateTab6" => Action::ActivateTab(6),
        "ActivateTab7" => Action::ActivateTab(7),
        "ActivateTab8" => Action::ActivateTab(8),
        "ActivateTab9" => Action::ActivateTab(9),
        _ => {
            return semantic(
                path,
                &format!("keybindings[{index}].action"),
                format!("unknown action {}", escape_diagnostic(&raw.action)),
            );
        }
    };
    Ok(KeyBinding {
        chord: crate::input::shortcut::KeyChord {
            key,
            modifiers: modifier_mask(&mods),
        },
        action,
        origin: crate::input::shortcut::BindingOrigin::User { index },
    })
}

fn modifier_mask(modifiers: &[Modifier]) -> leyline_gfx::ModifierMask {
    let mut mask = leyline_gfx::ModifierMask::empty();
    for modifier in modifiers {
        mask.insert(match modifier {
            Modifier::Control => leyline_gfx::ModifierMask::CONTROL,
            Modifier::Shift => leyline_gfx::ModifierMask::SHIFT,
            Modifier::Alt => leyline_gfx::ModifierMask::ALT,
            Modifier::Super => leyline_gfx::ModifierMask::SUPER,
        });
    }
    mask
}

fn chord(binding: &KeyBinding) -> crate::input::shortcut::KeyChord {
    binding.chord
}

fn semantic<T>(path: &Path, field: &str, reason: String) -> Result<T, ConfigError> {
    Err(ConfigError::Semantic {
        path: path.into(),
        field: field.into(),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Env {
        xdg: Option<OsString>,
        home: Option<OsString>,
    }
    impl ConfigEnvironment for Env {
        fn xdg_config_home(&self) -> Option<OsString> {
            self.xdg.clone()
        }
        fn home(&self) -> Option<OsString> {
            self.home.clone()
        }
    }

    #[test]
    fn defaults_include_system_clipboard_and_primary_bindings_once() {
        use crate::input::shortcut::LogicalKeyPattern;
        let bindings = default_keybindings();
        for (key, action) in [
            (LogicalKeyPattern::Character('c'), Action::CopyClipboard),
            (LogicalKeyPattern::Character('v'), Action::PasteClipboard),
        ] {
            assert_eq!(
                bindings
                    .iter()
                    .filter(|binding| {
                        binding.chord.key == key
                            && binding.chord.modifiers
                                == modifier_mask(&[Modifier::Control, Modifier::Shift])
                            && binding.action == action
                    })
                    .count(),
                1
            );
        }
        assert!(bindings.iter().any(|binding| {
            binding.chord.key == LogicalKeyPattern::Insert
                && binding.chord.modifiers == leyline_gfx::ModifierMask::SHIFT
                && binding.action == Action::PastePrimary
        }));
    }

    #[test]
    fn default_font_shortcuts_match_the_logical_characters_users_type() {
        use crate::input::shortcut::LogicalKeyPattern;
        let bindings = default_keybindings();
        assert!(bindings.iter().any(|binding| {
            binding.chord.key == LogicalKeyPattern::Character('=')
                && binding.chord.modifiers == leyline_gfx::ModifierMask::CONTROL
                && binding.action == Action::IncreaseFontSize
        }));
        assert!(bindings.iter().any(|binding| {
            binding.chord.key == LogicalKeyPattern::Character('-')
                && binding.chord.modifiers == leyline_gfx::ModifierMask::CONTROL
                && binding.action == Action::DecreaseFontSize
        }));
        assert!(bindings.iter().any(|binding| {
            binding.chord.key == LogicalKeyPattern::Character('0')
                && binding.chord.modifiers == leyline_gfx::ModifierMask::CONTROL
                && binding.action == Action::ResetFontSize
        }));
    }

    #[test]
    fn clipboard_actions_parse_from_user_bindings() {
        let source = "[[keybindings]]\nkey=\"C\"\nmods=[\"Control\",\"Shift\"]\naction=\"CopyClipboard\"\n[[keybindings]]\nkey=\"V\"\nmods=[\"Control\",\"Shift\"]\naction=\"PasteClipboard\"\n";
        let raw: RawConfig = toml::from_str(source).expect("raw config");
        let (effective, _) = raw
            .into_effective(Path::new("config.toml"), source)
            .expect("effective config");
        assert!(
            effective
                .keybindings
                .iter()
                .any(|binding| binding.action == Action::CopyClipboard)
        );
        assert!(
            effective
                .keybindings
                .iter()
                .any(|binding| binding.action == Action::PasteClipboard)
        );
    }

    #[test]
    fn xdg_path_precedes_home_and_relative_values_fall_back() {
        let source = FileConfigSource::new(Env {
            xdg: Some("/xdg".into()),
            home: Some("/home/me".into()),
        });
        assert_eq!(
            source.locate().expect("location"),
            ConfigLocation::Missing("/xdg/leyline/config.toml".into())
        );
        let source = FileConfigSource::new(Env {
            xdg: Some("relative".into()),
            home: Some("/home/me".into()),
        });
        assert_eq!(
            source.locate().expect("location"),
            ConfigLocation::Missing("/home/me/.config/leyline/config.toml".into())
        );
    }

    #[test]
    fn validates_and_merges_configuration() {
        let ansi = (0..16)
            .map(|index| format!("\"#{index:02x}{index:02x}{index:02x}\""))
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            "[font]\nsize=72\nhinting=\"full\"\nantialiasing=\"system\"\nline_spacing=2.5\n[colors]\nforeground=\"#01020304\"\nansi=[{ansi}]\n[scrollbar]\nmode=\"always\"\nwidth=5\nhit_width=14\nmin_thumb_size=20\n[window]\npadding_x=0\n[[keybindings]]\nkey=\"PageUp\"\nmods=[\"Shift\"]\naction=\"ScrollPageUp\"\n"
        );
        let raw: RawConfig = toml::from_str(&source).expect("raw config");
        let (effective, warnings) = raw
            .into_effective(Path::new("config.toml"), "")
            .expect("effective config");
        assert!((effective.font.size - 72.0).abs() < f64::EPSILON);
        assert_eq!(effective.colors.foreground, Color(0x0102_0304));
        assert_eq!(effective.window.padding_x, 0);
        assert_eq!(effective.font.hinting, HintingPreference::Full);
        assert_eq!(effective.font.antialiasing, AntialiasPreference::System);
        assert!((effective.font.line_spacing - 2.5).abs() < f64::EPSILON);
        assert_eq!(effective.colors.ansi[15], Color(0x0f0f_0fff));
        assert_eq!(effective.scrollbar.mode, ScrollbarMode::Always);
        assert!(effective.keybindings.iter().any(|binding| binding.chord.key
            == crate::input::shortcut::LogicalKeyPattern::PageUp
            && binding.action == Action::ScrollPageUp));
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn rejects_transparent_or_wrong_length_ansi_palettes_and_narrow_hit_targets() {
        for source in [
            "[colors]\nansi=[\"#01020304\"]\n",
            "[colors]\nansi=[\"#010203\"]\n",
            "[scrollbar]\nwidth=12\nhit_width=4\n",
        ] {
            let raw: RawConfig = toml::from_str(source).unwrap();
            assert!(
                raw.into_effective(Path::new("config.toml"), source)
                    .is_err()
            );
        }
    }

    #[test]
    fn shipped_reference_and_legacy_profiles_validate() {
        for source in [
            include_str!("../../../config/reference.toml"),
            include_str!("../../../config/legacy.toml"),
        ] {
            let raw: RawConfig = toml::from_str(source).unwrap();
            let (_, warnings) = raw
                .into_effective(Path::new("profile.toml"), source)
                .unwrap();
            assert!(warnings.is_empty());
        }
    }

    #[test]
    fn unknown_field_warns_with_unique_suggestion() {
        let raw: RawConfig = toml::from_str("[font]\nszie=12\n").expect("raw config");
        let (_, warnings) = raw
            .into_effective(Path::new("config.toml"), "[font]\nszie=12\n")
            .expect("effective config");
        assert!(
            warnings[0]
                .message
                .contains("font.szie at line 2; did you mean size?")
        );
    }

    #[test]
    fn rejects_invalid_resource_values() {
        let raw: RawConfig =
            toml::from_str("[scrolling]\nhistory_lines=100001\n").expect("raw config");
        let error = raw
            .into_effective(Path::new("config.toml"), "")
            .expect_err("must reject");
        assert!(error.to_string().contains("scrolling.history_lines"));
    }

    #[test]
    fn rejects_unknown_key_names_during_configuration_loading() {
        let raw: RawConfig = toml::from_str(
            "[[keybindings]]\nkey=\"DefinitelyNotAKey\"\nmods=[]\naction=\"PastePrimary\"\n",
        )
        .expect("raw config");
        let error = raw
            .into_effective(Path::new("config.toml"), "")
            .expect_err("unknown key must fail");
        assert!(error.to_string().contains("keybindings[0].key"));
    }

    #[test]
    fn tabs_are_bounded_and_tab_actions_are_configurable() {
        let source = "[tabs]\nmax_count=12\nbar_height=36\nmin_width=72\nmax_width=200\nshow_close_button=false\n[[keybindings]]\nkey=\"F1\"\nmods=[]\naction=\"ActivateTab9\"\n";
        let raw: RawConfig = toml::from_str(source).expect("raw config");
        let (effective, _) = raw
            .into_effective(Path::new("config.toml"), source)
            .expect("effective config");
        assert_eq!(effective.tabs.max_count, 12);
        assert!(!effective.tabs.show_close_button);
        assert!(
            effective
                .keybindings
                .iter()
                .any(|binding| binding.action == Action::ActivateTab(9))
        );

        for source in [
            "[tabs]\nmax_count=33\n",
            "[tabs]\nmin_width=200\nmax_width=100\n",
        ] {
            let raw: RawConfig = toml::from_str(source).expect("raw config");
            assert!(
                raw.into_effective(Path::new("config.toml"), source)
                    .is_err()
            );
        }
    }

    #[test]
    fn new_tab_cwd_policies_validate_fixed_paths_and_warn_when_ignored() {
        let source = "[tabs]\nnew_tab_cwd='fixed'\nnew_tab_fixed_cwd='/srv/project'\n";
        let raw: RawConfig = toml::from_str(source).unwrap();
        let (effective, warnings) = raw
            .into_effective(Path::new("config.toml"), source)
            .unwrap();
        assert_eq!(
            effective.tabs.new_tab_cwd,
            NewTabCwdPolicy::Fixed(PathBuf::from("/srv/project"))
        );
        assert!(warnings.is_empty());

        for source in [
            "[tabs]\nnew_tab_cwd='fixed'\n",
            "[tabs]\nnew_tab_cwd='fixed'\nnew_tab_fixed_cwd='relative'\n",
            "[tabs]\nnew_tab_cwd='other'\n",
        ] {
            let raw: RawConfig = toml::from_str(source).unwrap();
            assert!(
                raw.into_effective(Path::new("config.toml"), source)
                    .is_err()
            );
        }

        let source = "[tabs]\nnew_tab_cwd='home'\nnew_tab_fixed_cwd='/ignored'\n";
        let raw: RawConfig = toml::from_str(source).unwrap();
        let (effective, warnings) = raw
            .into_effective(Path::new("config.toml"), source)
            .unwrap();
        assert_eq!(effective.tabs.new_tab_cwd, NewTabCwdPolicy::Home);
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn bell_configuration_is_bounded_and_mute_action_is_bindable() {
        assert!(EffectiveConfig::default().bell.desktop_notifications);
        let source = "[bell]\nvisual_duration_ms=1000\nnotification_cooldown_seconds=3600\nnotification_burst_per_minute=30\ndesktop_notifications=true\n[[keybindings]]\nkey='F2'\nmods=[]\naction='ToggleBellMute'\n";
        let raw: RawConfig = toml::from_str(source).unwrap();
        let (effective, warnings) = raw
            .into_effective(Path::new("config.toml"), source)
            .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(effective.bell.visual_duration, Duration::from_secs(1));
        assert_eq!(
            effective.bell.notification_cooldown,
            Duration::from_hours(1)
        );
        assert_eq!(effective.bell.notification_burst_per_minute.get(), 30);
        assert!(effective.bell.desktop_notifications);
        assert!(
            effective
                .keybindings
                .iter()
                .any(|binding| binding.action == Action::ToggleBellMute)
        );

        for source in [
            "[bell]\nvisual_duration_ms=39\n",
            "[bell]\nnotification_cooldown_seconds=0\n",
            "[bell]\nnotification_burst_per_minute=31\n",
        ] {
            let raw: RawConfig = toml::from_str(source).unwrap();
            assert!(
                raw.into_effective(Path::new("config.toml"), source)
                    .is_err()
            );
        }
    }
}
