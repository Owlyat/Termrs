//! Load + defaults for `config.toml` (general, window, keybindings).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// General emulator settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct General {
    /// Shell to spawn per pane. Empty = auto (`cmd.exe` on Windows, `$SHELL`/`sh` else).
    #[serde(default)]
    pub shell: String,
    /// vt100 scrollback lines kept per pane.
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
    /// Path to a monospace `.ttf`/`.otf` for the window renderer.
    /// Empty = auto-detect a system monospace font.
    #[serde(default)]
    pub font: String,
    /// Rendered glyph height in pixels for the window renderer.
    #[serde(default = "default_font_size")]
    pub font_size: u32,
    /// Extra fonts tried for glyphs the primary font lacks (Nerd Font icons,
    /// symbols, emoji). Empty = auto-detect one.
    #[serde(default)]
    pub font_fallback: Vec<String>,
    /// TCP port for a pane's MCP server. `0` = pick a free port each time;
    /// set a fixed value so an MCP client (e.g. opencode) can hardcode the URL.
    #[serde(default = "default_mcp_port")]
    pub mcp_port: u16,
}

fn default_scrollback() -> usize {
    2000
}

fn default_font_size() -> u32 {
    16
}

fn default_mcp_port() -> u16 {
    7331
}

impl Default for General {
    fn default() -> Self {
        Self {
            shell: String::new(),
            scrollback: default_scrollback(),
            font: String::new(),
            font_size: default_font_size(),
            font_fallback: Vec::new(),
            mcp_port: default_mcp_port(),
        }
    }
}

/// Native window settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Image shown behind the terminal (wallpaper). Empty = none.
    #[serde(default)]
    pub background_image: String,
    /// Terminal layer opacity over the background image, 0.0..=1.0.
    /// 1.0 = solid (default). Without an image this has no visible effect:
    /// the GPU pipeline renders opaque cells, so the desktop cannot show
    /// through; the image is the translucency source.
    #[serde(default = "default_opacity")]
    pub opacity: f32,
    /// Window title bar + borders. `false` = borderless window.
    #[serde(default = "default_decorations")]
    pub decorations: bool,
    /// Ask the OS for a transparent window so the desktop shows through the
    /// terminal's background (text stays opaque). Needs a compositor that
    /// supports premultiplied alpha. Not honored on Windows: the GPU backend
    /// only offers opaque swapchains there, so the terminal stays opaque
    /// (a warning is logged at startup). Changing it needs a restart.
    #[serde(default)]
    pub transparent: bool,
}

fn default_opacity() -> f32 {
    1.0
}

fn default_decorations() -> bool {
    true
}

impl Default for Window {
    fn default() -> Self {
        Self {
            background_image: String::new(),
            opacity: default_opacity(),
            decorations: default_decorations(),
            transparent: false,
        }
    }
}

impl Window {
    /// Clamp opacity into range; non-finite values fall back to solid.
    pub fn clamped_opacity(&self) -> f32 {
        if !self.opacity.is_finite() {
            return 1.0;
        }
        self.opacity.clamp(0.0, 1.0)
    }
}

/// Terminal palette, as `#rrggbb` strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    #[serde(default = "th_fg")]
    pub foreground: String,
    #[serde(default = "th_bg")]
    pub background: String,
    /// Cursor cell color. Empty = invert the cell (block) / foreground (bar).
    #[serde(default)]
    pub cursor: String,
    /// Cursor shape: `bar` (default), `block` or `underline`.
    #[serde(default = "th_cursor_shape")]
    pub cursor_shape: String,
    /// Focused pane border. Empty = yellow.
    #[serde(default)]
    pub accent: String,
}

fn th_fg() -> String {
    "#dcdcdc".into()
}

fn th_bg() -> String {
    "#101014".into()
}

fn th_cursor_shape() -> String {
    "bar".into()
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: th_fg(),
            background: th_bg(),
            cursor: String::new(),
            cursor_shape: th_cursor_shape(),
            accent: String::new(),
        }
    }
}

/// How the terminal cursor is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Bar,
    Block,
    Underline,
}

/// Theme with hex strings resolved once at startup.
#[derive(Debug, Clone, Copy)]
pub struct ThemeColors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub cursor: Option<[u8; 3]>,
    pub cursor_shape: CursorShape,
    pub accent: Option<[u8; 3]>,
}

impl Theme {
    /// Resolve hex strings; bad values warn once and fall back.
    pub fn resolve(&self) -> ThemeColors {
        ThemeColors {
            fg: parse_hex(&self.foreground, "theme.foreground", [0xdc, 0xdc, 0xdc]),
            bg: parse_hex(&self.background, "theme.background", [0x10, 0x10, 0x14]),
            cursor: parse_hex_opt(&self.cursor, "theme.cursor"),
            cursor_shape: match self.cursor_shape.trim().to_ascii_lowercase().as_str() {
                "block" => CursorShape::Block,
                "underline" => CursorShape::Underline,
                _ => CursorShape::Bar,
            },
            accent: parse_hex_opt(&self.accent, "theme.accent"),
        }
    }
}

/// Parse `#rrggbb` (leading `#` optional); `None` for empty strings.
fn parse_hex_opt(s: &str, what: &str) -> Option<[u8; 3]> {
    if s.trim().is_empty() {
        return None;
    }
    match parse_hex_str(s) {
        Some(rgb) => Some(rgb),
        None => {
            eprintln!("termrs: bad {what} {s:?}; ignoring");
            None
        }
    }
}

/// Parse with fallback + single warning.
fn parse_hex(s: &str, what: &str, fallback: [u8; 3]) -> [u8; 3] {
    match parse_hex_str(s) {
        Some(rgb) => rgb,
        None => {
            eprintln!("termrs: bad {what} {s:?}; using default");
            fallback
        }
    }
}

fn parse_hex_str(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// A typed-text macro: key binding(s) that send text to the focused pane,
/// like wezterm's `Multiple { SendString, SendKey Enter }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    /// Key spec(s) triggering the macro, e.g. `["ctrl+B"]`.
    #[serde(default, deserialize_with = "string_or_list")]
    pub keys: Vec<String>,
    /// Text typed into the focused pane.
    #[serde(default)]
    pub send: String,
    /// Whether to press Enter after the text.
    #[serde(default = "default_true")]
    pub enter: bool,
}

/// Which transition a queued effect belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxKind {
    /// New split pane appears.
    Fresh,
    /// Pane focus moves.
    Focus,
    /// Pane is closing (plays on its rect before removal).
    Close,
}

/// Graphic transitions (`tachyonfx`). Effect names are `dissolve`, `fade`,
/// `flash` or `none`. `fade` is direction-aware: fade-in for fresh/focus,
/// fade-out for close. Durations are milliseconds (0 disables that effect).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxConfig {
    /// Master switch: `false` disables every transition.
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "fx_open")]
    pub open: String,
    #[serde(default = "fx_open_ms")]
    pub open_ms: u32,
    #[serde(default = "fx_close")]
    pub close: String,
    #[serde(default = "fx_close_ms")]
    pub close_ms: u32,
    #[serde(default = "fx_focus")]
    pub focus: String,
    #[serde(default = "fx_focus_ms")]
    pub focus_ms: u32,
}

fn default_true() -> bool {
    true
}

fn fx_open() -> String {
    "dissolve".into()
}

fn fx_open_ms() -> u32 {
    450
}

fn fx_close() -> String {
    "dissolve".into()
}

fn fx_close_ms() -> u32 {
    350
}

fn fx_focus() -> String {
    "flash".into()
}

fn fx_focus_ms() -> u32 {
    300
}

impl Default for FxConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            open: fx_open(),
            open_ms: fx_open_ms(),
            close: fx_close(),
            close_ms: fx_close_ms(),
            focus: fx_focus(),
            focus_ms: fx_focus_ms(),
        }
    }
}

impl FxConfig {
    /// Resolve `(effect name, milliseconds)` for a transition, or `None`
    /// when disabled, unknown, or zero-duration.
    pub fn resolve(&self, kind: FxKind) -> Option<(String, u32)> {
        if !self.enabled {
            return None;
        }
        let (name, ms) = match kind {
            FxKind::Fresh => (&self.open, self.open_ms),
            FxKind::Close => (&self.close, self.close_ms),
            FxKind::Focus => (&self.focus, self.focus_ms),
        };
        let name = name.trim().to_lowercase();
        if !matches!(name.as_str(), "dissolve" | "fade" | "flash") {
            return None;
        }
        let ms = ms.clamp(0, 2000);
        if ms == 0 {
            return None;
        }
        Some((name, ms))
    }
}

/// Keybindings. Each action takes one spec string or a list of them, e.g.
/// `quit = "ctrl+q"` or `quit = ["ctrl+q", "alt+q"]`. A spec looks like
/// `ctrl+q`, `alt+enter`, `f1`, `shift+up`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keys {
    #[serde(default = "k_split_h", deserialize_with = "string_or_list")]
    pub split_horizontal: Vec<String>,
    #[serde(default = "k_split_v", deserialize_with = "string_or_list")]
    pub split_vertical: Vec<String>,
    #[serde(default = "k_close", deserialize_with = "string_or_list")]
    pub close_pane: Vec<String>,
    #[serde(default = "k_next", deserialize_with = "string_or_list")]
    pub focus_next: Vec<String>,
    #[serde(default = "k_prev", deserialize_with = "string_or_list")]
    pub focus_prev: Vec<String>,
    #[serde(default = "k_quit", deserialize_with = "string_or_list")]
    pub quit: Vec<String>,
    #[serde(default = "k_menu", deserialize_with = "string_or_list")]
    pub workspace_menu: Vec<String>,
    #[serde(default = "k_scroll", deserialize_with = "string_or_list")]
    pub scroll_view: Vec<String>,
    #[serde(default = "k_image", deserialize_with = "string_or_list")]
    pub view_image: Vec<String>,
    #[serde(default = "k_ws_new", deserialize_with = "string_or_list")]
    pub workspace_new: Vec<String>,
    #[serde(default = "k_ws_next", deserialize_with = "string_or_list")]
    pub workspace_next: Vec<String>,
    #[serde(default = "k_ws_prev", deserialize_with = "string_or_list")]
    pub workspace_prev: Vec<String>,
    #[serde(default = "k_help", deserialize_with = "string_or_list")]
    pub help: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub focus_left: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub focus_right: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub focus_up: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub focus_down: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub resize_left: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub resize_right: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub resize_up: Vec<String>,
    #[serde(default = "k_empty", deserialize_with = "string_or_list")]
    pub resize_down: Vec<String>,
    #[serde(default = "k_zoom_in", deserialize_with = "string_or_list")]
    pub zoom_in: Vec<String>,
    #[serde(default = "k_zoom_out", deserialize_with = "string_or_list")]
    pub zoom_out: Vec<String>,
    #[serde(default = "k_zoom_reset", deserialize_with = "string_or_list")]
    pub zoom_reset: Vec<String>,
    #[serde(default = "k_yank", deserialize_with = "string_or_list")]
    pub yank_last: Vec<String>,
    #[serde(default = "k_select", deserialize_with = "string_or_list")]
    pub select_mode: Vec<String>,
    #[serde(default = "k_edit_config", deserialize_with = "string_or_list")]
    pub edit_config: Vec<String>,
    #[serde(default = "k_palette", deserialize_with = "string_or_list")]
    pub command_palette: Vec<String>,
    #[serde(default = "k_paste", deserialize_with = "string_or_list")]
    pub paste: Vec<String>,
    #[serde(default = "k_cmd_picker", deserialize_with = "string_or_list")]
    pub commands_picker: Vec<String>,
    #[serde(default = "k_cmd_save", deserialize_with = "string_or_list")]
    pub command_save: Vec<String>,
    #[serde(default = "k_pane_zoom", deserialize_with = "string_or_list")]
    pub pane_zoom: Vec<String>,
}

/// Accept either `"ctrl+q"` or `["ctrl+q", "alt+q"]` for a keybinding.
fn string_or_list<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StringOrList;
    impl<'de> serde::de::Visitor<'de> for StringOrList {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a key spec string or a list of key spec strings")
        }
        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Vec<String>, E> {
            Ok(vec![v.to_owned()])
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Vec<String>, A::Error> {
            let mut out = Vec::new();
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }
    d.deserialize_any(StringOrList)
}

fn k_split_h() -> Vec<String> {
    // `ctrl+shift+alt+s` is an explicit alternate: on AltGr layouts Ctrl+Alt
    // is the AltGr level, and Shift is ignored for character keys, so plain
    // `ctrl+s` already answers to Ctrl+Shift+S.
    vec!["ctrl+s".into(), "ctrl+shift+alt+s".into()]
}
fn k_split_v() -> Vec<String> {
    vec!["ctrl+v".into()]
}
fn k_close() -> Vec<String> {
    vec!["ctrl+w".into()]
}
fn k_next() -> Vec<String> {
    vec!["alt+j".into()]
}
fn k_prev() -> Vec<String> {
    vec!["alt+k".into()]
}
fn k_quit() -> Vec<String> {
    vec!["ctrl+q".into()]
}
fn k_menu() -> Vec<String> {
    vec!["ctrl+o".into()]
}
fn k_scroll() -> Vec<String> {
    vec!["ctrl+shift+up".into()]
}
fn k_image() -> Vec<String> {
    vec!["ctrl+i".into()]
}
fn k_ws_new() -> Vec<String> {
    vec!["ctrl+t".into()]
}
fn k_ws_next() -> Vec<String> {
    vec!["alt+l".into()]
}
fn k_ws_prev() -> Vec<String> {
    vec!["alt+h".into()]
}
fn k_help() -> Vec<String> {
    vec!["f1".into()]
}

fn k_empty() -> Vec<String> {
    Vec::new()
}

fn k_zoom_in() -> Vec<String> {
    vec!["ctrl+plus".into()]
}

fn k_zoom_out() -> Vec<String> {
    vec!["ctrl+minus".into()]
}

fn k_zoom_reset() -> Vec<String> {
    vec!["ctrl+0".into()]
}

fn k_yank() -> Vec<String> {
    vec!["ctrl+y".into()]
}

fn k_select() -> Vec<String> {
    vec!["ctrl+shift+space".into()]
}

fn k_edit_config() -> Vec<String> {
    vec!["ctrl+e".into()]
}

fn k_palette() -> Vec<String> {
    vec!["ctrl+p".into()]
}

fn k_paste() -> Vec<String> {
    vec!["ctrl+shift+v".into()]
}

fn k_cmd_picker() -> Vec<String> {
    vec!["ctrl+r".into()]
}

fn k_cmd_save() -> Vec<String> {
    vec!["ctrl+shift+r".into()]
}

fn k_pane_zoom() -> Vec<String> {
    vec!["ctrl+shift+z".into()]
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            split_horizontal: k_split_h(),
            split_vertical: k_split_v(),
            close_pane: k_close(),
            focus_next: k_next(),
            focus_prev: k_prev(),
            quit: k_quit(),
            workspace_menu: k_menu(),
            scroll_view: k_scroll(),
            view_image: k_image(),
            workspace_new: k_ws_new(),
            workspace_next: k_ws_next(),
            workspace_prev: k_ws_prev(),
            help: k_help(),
            focus_left: k_empty(),
            focus_right: k_empty(),
            focus_up: k_empty(),
            focus_down: k_empty(),
            resize_left: k_empty(),
            resize_right: k_empty(),
            resize_up: k_empty(),
            resize_down: k_empty(),
            zoom_in: k_zoom_in(),
            zoom_out: k_zoom_out(),
            zoom_reset: k_zoom_reset(),
            yank_last: k_yank(),
            select_mode: k_select(),
            edit_config: k_edit_config(),
            command_palette: k_palette(),
            paste: k_paste(),
            commands_picker: k_cmd_picker(),
            command_save: k_cmd_save(),
            pane_zoom: k_pane_zoom(),
        }
    }
}

/// AI assistant used by the "Ask AI" palette command.
///
/// `command` is any CLI that reads a prompt on stdin and writes the answer
/// on stdout (e.g. `ollama run llama3`, `llm`, `aichat`, `sgpt`). Empty
/// disables the feature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ai {
    #[serde(default)]
    pub command: String,
    /// System/lead-in text stating the goal, prepended to the question.
    /// The model queries saved commands / installed tools on demand.
    #[serde(default = "default_ai_prompt")]
    pub prompt: String,
    /// Let the AI query the saved command database (`{"action":"db",...}`).
    /// Only queried rows are sent back, never the whole database.
    #[serde(default = "yes")]
    pub use_command_db: bool,
    /// Seconds before the AI CLI is killed per turn (each of the agentic
    /// turns gets its own budget). Small local models need room to think;
    /// hung runs fail fast instead of wedging the worker.
    #[serde(default = "default_ai_timeout")]
    pub timeout_s: u64,
}

fn yes() -> bool {
    true
}

fn default_ai_timeout() -> u64 {
    60
}

fn default_ai_prompt() -> String {
    "Goal: create a single shell command for the target shell (see the Target shell line) satisfying the user's need. You may first gather context with exactly one JSON object per turn -- {\"action\":\"db\",\"query\":\"<keywords>\",\"limit\":10} searches the saved command database, {\"action\":\"which\",\"tools\":[\"<name>\"]} checks whether a CLI tool is installed, {\"action\":\"help\",\"tool\":\"<name>\"} reads a tool's usage (the tool itself is never executed). Then answer with {\"action\":\"answer\",\"command\":\"<single shell command>\"} (or ```answer <command> ```). Rules: you cannot run commands and nothing runs except your final answer, typed into the pane for the user to confirm. Final answer must be one runnable shell line for the target shell.".into()
}

impl Default for Ai {
    fn default() -> Self {
        Self {
            command: String::new(),
            prompt: default_ai_prompt(),
            use_command_db: true,
            timeout_s: default_ai_timeout(),
        }
    }
}

/// Saved-command database settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[derive(Default)]
pub struct Commands {
    /// Path to the SQLite database. Empty = `<config-dir>/commands.db`.
    #[serde(default)]
    pub db: String,
}


/// Mouse settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mouse {
    /// Forward mouse events to apps that request tracking (vim, tmux,
    /// TUIs) and open URLs on modifier+click. Off = termrs handles all
    /// clicks itself (focus panes, scroll) as before.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Modifier held while clicking a URL to open it in the default
    /// browser: one of "ctrl" (default), "alt" or "shift". Anything else
    /// falls back to ctrl.
    #[serde(default = "default_url_mod")]
    pub url_mod: String,
}

fn default_url_mod() -> String {
    "ctrl".into()
}

impl Default for Mouse {
    fn default() -> Self {
        Self {
            enabled: true,
            url_mod: default_url_mod(),
        }
    }
}

/// Terminal sharing settings (`Share terminal` palette command).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Share {
    /// Public URL of the share page (the wasm iroh client + xterm.js). The
    /// copied link is `page_url#ticket=…&code=…`. Empty = the ticket itself is
    /// copied (paste it into a locally opened copy of the page).
    #[serde(default = "default_share_page_url")]
    pub page_url: String,
    /// Let the first authenticated viewer type; any others watch. `false`
    /// makes every viewer read-only.
    #[serde(default = "default_true")]
    pub allow_control: bool,
    /// Length of the generated session code (clamped to 4..=16).
    #[serde(default = "default_share_code_len")]
    pub code_len: usize,
}

fn default_share_code_len() -> usize {
    6
}

/// Public URL of the termrs share page (GitHub Pages).
fn default_share_page_url() -> String {
    "https://Owlyat.github.io/Termrs/".into()
}

impl Default for Share {
    fn default() -> Self {
        Self {
            page_url: default_share_page_url(),
            allow_control: true,
            code_len: default_share_code_len(),
        }
    }
}

/// Root config file shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default)]
    pub window: Window,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub ai: Ai,
    #[serde(default)]
    pub commands: Commands,
    #[serde(default)]
    pub fx: FxConfig,
    #[serde(default)]
    pub keys: Keys,
    /// Typed-text macros (`[[macros]]`).
    #[serde(default)]
    pub macros: Vec<Macro>,
    #[serde(default)]
    pub mouse: Mouse,
    #[serde(default)]
    pub share: Share,
    /// Path this config was loaded from (not serialized).
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

impl Config {
    /// Serialize built-in defaults (for `--print-default-config`).
    pub fn default_toml() -> String {
        toml::to_string_pretty(&Config {
            general: General::default(),
            window: Window::default(),
            theme: Theme::default(),
            ai: Ai::default(),
            commands: Commands::default(),
            fx: FxConfig::default(),
            keys: Keys::default(),
            macros: Vec::new(),
            mouse: Mouse::default(),
            share: Share::default(),
            source: None,
        })
        .unwrap_or_default()
    }

    /// Where the saved pane layout lives: next to the config when known,
    /// else `layout.toml` in the current directory.
    pub fn layout_path(&self) -> PathBuf {
        match &self.source {
            Some(p) => p.with_file_name("layout.toml"),
            None => PathBuf::from("layout.toml"),
        }
    }

    /// Directory holding app state: the config's own directory when known,
    /// else `~/.config/termrs` (created on demand).
    pub fn state_dir(&self) -> PathBuf {
        if let Some(parent) = self.source.as_ref().and_then(|p| p.parent())
            && !parent.as_os_str().is_empty() {
                return parent.to_path_buf();
            }
        Self::home_dir().unwrap_or_else(|| PathBuf::from("."))
    }

    /// `termrs.log` inside [`Config::state_dir`] (directory is created).
    pub fn log_path(&self) -> PathBuf {
        let dir = self.state_dir();
        let _ = std::fs::create_dir_all(&dir);
        dir.join("termrs.log")
    }

    /// SQLite file for saved commands: `[commands] db` when set, else
    /// `commands.db` in [`Config::state_dir`].
    pub fn commands_db_path(&self) -> PathBuf {
        let db = self.commands.db.trim();
        if db.is_empty() {
            self.state_dir().join("commands.db")
        } else {
            PathBuf::from(db)
        }
    }

    /// Persist the in-memory `[general] font` and `font_size` values back to
    /// the config file this config was loaded from. The file is edited
    /// line-wise so comments and formatting survive; only the two relevant
    /// lines change (inserted when missing). Returns the written path.
    pub fn write_font_settings(&self) -> Result<PathBuf, String> {
        let Some(path) = self.source.as_ref() else {
            return Err("no config file loaded (use -c <path>)".into());
        };
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let font = toml::Value::String(self.general.font.clone()).to_string();
        let out = set_general_key(&text, "font", &font);
        let size = self.general.font_size.max(6).to_string();
        let out = set_general_key(&out, "font_size", &size);
        std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(path.clone())
    }

    /// Parse config text, keeping [`Config::source`] on the result.
    /// Returns the human-readable error (with line/column) on failure.
    pub fn parse_with_source(text: &str, source: Option<PathBuf>) -> Result<Self, String> {
        match toml::from_str::<Config>(text) {
            Ok(mut cfg) => {
                if cfg.general.scrollback == 0 {
                    cfg.general.scrollback = default_scrollback();
                }
                cfg.source = source;
                Ok(cfg)
            }
            Err(e) => Err(format!("{e}")),
        }
    }

    /// Home config dir: `~/.config/termrs` (`%USERPROFILE%` preferred on
    /// Windows, else `$HOME`). The canonical home for `config.toml` and,
    /// by default, `layout.toml`, `commands.db` and `termrs.log`.
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(PathBuf::from)
            .map(|base| base.join(".config").join("termrs"))
            .ok()
    }

    /// Canonical config file: `~/.config/termrs/config.toml`, if a home
    /// directory is known.
    pub fn default_path() -> Option<PathBuf> {
        Self::home_dir().map(|d| d.join("config.toml"))
    }

    /// Candidate paths searched when `--config` is absent. The `.config`
    /// home wins; repo/cwd/exe locations stay as dev fallbacks.
    pub fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(home_cfg) = Self::default_path() {
            out.push(home_cfg);
        }
        out.push(PathBuf::from("config.toml"));
        out.push(PathBuf::from("crates/termrs/config.toml"));
        if let Ok(cwd) = std::env::current_dir() {
            let p = cwd.join("config.toml");
            if !out.contains(&p) {
                out.push(p);
            }
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent() {
                let p = dir.join("config.toml");
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        out
    }

    /// Write built-in defaults to the `.config` home so users have a real
    /// file to edit. Best effort: failures are reported, never fatal.
    fn bootstrap_default() -> Option<PathBuf> {
        Self::bootstrap_default_at(&Self::default_path()?)
    }

    /// Write defaults to `path` (creating parent dirs), unless it exists.
    /// Split out for tests so they never touch the real home directory.
    fn bootstrap_default_at(path: &std::path::Path) -> Option<PathBuf> {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_err() {
                return None;
            }
        match std::fs::write(path, Self::default_toml()) {
            Ok(()) => {
                eprintln!("termrs: wrote default config to {}", path.display());
                Some(path.to_path_buf())
            }
            Err(e) => {
                eprintln!(
                    "termrs: cannot write default config to {}: {e}",
                    path.display()
                );
                None
            }
        }
    }

    /// Load from explicit path, else first existing candidate (`.config`
    /// home first), else bootstrap defaults into `.config`, else defaults.
    pub fn load(explicit: Option<&std::path::Path>) -> Self {
        if let Some(p) = explicit {
            return Self::load_file(p);
        }
        for c in Self::candidates() {
            if c.is_file() {
                return Self::load_file(&c);
            }
        }
        if let Some(path) = Self::bootstrap_default() {
            return Self::load_file(&path);
        }
        Self {
            general: General::default(),
            window: Window::default(),
            theme: Theme::default(),
            ai: Ai::default(),
            commands: Commands::default(),
            fx: FxConfig::default(),
            keys: Keys::default(),
            macros: Vec::new(),
            mouse: Mouse::default(),
            share: Share::default(),
            source: None,
        }
    }

    /// Load one file; missing/unparseable -> defaults (with stderr note).
    fn load_file(path: &std::path::Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => {
                return Self {
                    general: General::default(),
                    window: Window::default(),
            theme: Theme::default(),
            ai: Ai::default(),
            commands: Commands::default(),
                    fx: FxConfig::default(),
                    keys: Keys::default(),
            macros: Vec::new(),
            mouse: Mouse::default(),
                    share: Share::default(),
                    source: Some(path.to_path_buf()),
                };
            }
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut cfg) => {
                if cfg.general.scrollback == 0 {
                    cfg.general.scrollback = default_scrollback();
                }
                cfg.source = Some(path.to_path_buf());
                cfg
            }
            Err(e) => {
                eprintln!("termrs: bad config {}: {e}; using defaults", path.display());
                Self {
                    general: General::default(),
                    window: Window::default(),
            theme: Theme::default(),
            ai: Ai::default(),
            commands: Commands::default(),
                    fx: FxConfig::default(),
                    keys: Keys::default(),
            macros: Vec::new(),
            mouse: Mouse::default(),
                    share: Share::default(),
                    source: Some(path.to_path_buf()),
                }
            }
        }
    }
}

/// Set `key = value` inside the `[general]` section of TOML `text`,
/// preserving everything else byte-for-byte: an existing line is replaced,
/// otherwise the pair is inserted right after the `[general]` header (or a
/// new section is appended when there is none).
fn set_general_key(text: &str, key: &str, value: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let trailing_newline = text.ends_with('\n');
    let mut general_at: Option<usize> = None;
    let mut in_general = false;
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if t.starts_with('[') {
            in_general = t == "[general]";
            if in_general && general_at.is_none() {
                general_at = Some(i);
            }
            continue;
        }
        if in_general && is_key_line(line, key) {
            let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
            lines[i] = format!("{indent}{key} = {value}");
            return join_lines(&lines, trailing_newline);
        }
    }
    let fresh = format!("{key} = {value}");
    match general_at {
        Some(i) => {
            lines.insert(i + 1, fresh);
        }
        None => {
            if !lines.is_empty() && !lines.last().map(|l| l.trim().is_empty()).unwrap_or(true) {
                lines.push(String::new());
            }
            lines.push("[general]".to_string());
            lines.push(fresh);
        }
    }
    join_lines(&lines, trailing_newline)
}

/// True when `line` assigns `key` (`key = ...`, any spacing, not a comment).
fn is_key_line(line: &str, key: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with('#') {
        return false;
    }
    let Some(rest) = t.strip_prefix(key) else {
        return false;
    };
    // `key` must end on a boundary (`fontx` is not `font`).
    match rest.chars().next() {
        Some('=') => true,
        Some(c) if c.is_whitespace() => rest.trim_start().starts_with('='),
        _ => false,
    }
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let cfg: Config = toml::from_str(&Config::default_toml()).unwrap();
        assert_eq!(cfg.general.scrollback, 2000);
        assert_eq!(cfg.keys.quit, vec!["ctrl+q".to_string()]);
        assert_eq!(cfg.window.opacity, 1.0);
        assert!(cfg.window.decorations);
        assert!(cfg.window.background_image.is_empty());
        assert!(cfg.fx.enabled);
        assert_eq!(cfg.fx.resolve(FxKind::Fresh), Some(("dissolve".into(), 450)));
        assert_eq!(cfg.fx.resolve(FxKind::Close), Some(("dissolve".into(), 350)));
        assert_eq!(cfg.fx.resolve(FxKind::Focus), Some(("flash".into(), 300)));
        assert_eq!(cfg.ai.timeout_s, 60);
        assert_eq!(cfg.keys.pane_zoom, vec!["ctrl+shift+z".to_string()]);
        assert!(cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "ctrl");
    }

    /// A custom `[mouse]` section parses; unknown url mods are tolerated
    /// here (the app falls back to ctrl when interpreting them).
    #[test]
    fn mouse_section_parses() {
        let cfg: Config = toml::from_str("[mouse]\nenabled = false\nurl_mod = \"alt\"\n").unwrap();
        assert!(!cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "alt");
        // Absent section keeps defaults.
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert!(cfg.mouse.enabled);
        assert_eq!(cfg.mouse.url_mod, "ctrl");
    }

    #[test]
    fn share_section_parses() {
        let cfg: Config = toml::from_str(
            "[share]\npage_url = \"https://example.com/termrs\"\nallow_control = false\ncode_len = 10\n",
        )
        .unwrap();
        assert_eq!(cfg.share.page_url, "https://example.com/termrs");
        assert!(!cfg.share.allow_control);
        assert_eq!(cfg.share.code_len, 10);
        // Absent section keeps defaults.
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(cfg.share.page_url, "https://Owlyat.github.io/Termrs/");
        assert!(cfg.share.allow_control);
        assert_eq!(cfg.share.code_len, 6);
        // A `[share]` section without page_url also gets the default URL.
        let cfg: Config = toml::from_str("[share]\nallow_control = true\n").unwrap();
        assert_eq!(cfg.share.page_url, "https://Owlyat.github.io/Termrs/");
    }

    /// The `.config` home is the canonical config location and the first
    /// search candidate; bootstrapping writes parseable defaults there.
    #[test]
    fn config_home_is_dotconfig_first() {
        let home = Config::home_dir().expect("home dir known");
        assert!(
            home.ends_with(".config/termrs") || home.ends_with(".config\\termrs"),
            "home: {}",
            home.display()
        );
        let def = Config::default_path().expect("default path known");
        assert_eq!(def, home.join("config.toml"));
        let cands = Config::candidates();
        assert_eq!(cands.first(), Some(&def), "candidates: {cands:?}");

        // Bootstrap into an isolated temp dir (never the real home).
        let dir = std::env::temp_dir().join(format!(
            "termrs-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("termrs").join("config.toml");
        let written = Config::bootstrap_default_at(&path).expect("bootstrap");
        assert_eq!(written, path);
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: Config = toml::from_str(&text).expect("bootstrapped config parses");
        assert_eq!(cfg.ai.timeout_s, 60);
        // Second call keeps the existing file (does not overwrite edits).
        std::fs::write(&path, "[general]\nscrollback = 1234\n").unwrap();
        assert_eq!(Config::bootstrap_default_at(&path), Some(path.clone()));
        assert!(std::fs::read_to_string(&path).unwrap().contains("1234"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keys_accept_string_or_list() {
        let single: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(single.keys.quit, vec!["alt+q".to_string()]);
        let multi: Config =
            toml::from_str("[keys]\nquit = [\"ctrl+q\", \"alt+q\"]\n").unwrap();
        assert_eq!(
            multi.keys.quit,
            vec!["ctrl+q".to_string(), "alt+q".to_string()]
        );
    }

    #[test]
    fn theme_and_macros_parse() {
        let cfg: Config = toml::from_str(
            "[theme]\nforeground = \"#6d7655\"\nbackground = \"#1f1f1f\"\ncursor = \"#c3cea0\"\naccent = \"#c3cea0\"\n\
             [[macros]]\nkeys = [\"ctrl+B\"]\nsend = \"wsl w3m\"\nenter = true\n",
        )
        .unwrap();
        let colors = cfg.theme.resolve();
        assert_eq!(colors.fg, [0x6d, 0x76, 0x55]);
        assert_eq!(colors.bg, [0x1f, 0x1f, 0x1f]);
        assert_eq!(colors.cursor, Some([0xc3, 0xce, 0xa0]));
        assert_eq!(colors.accent, Some([0xc3, 0xce, 0xa0]));
        assert_eq!(colors.cursor_shape, CursorShape::Bar);
        assert_eq!(cfg.macros.len(), 1);
        assert_eq!(cfg.macros[0].keys, vec!["ctrl+B".to_string()]);
        assert_eq!(cfg.macros[0].send, "wsl w3m");
        assert!(cfg.macros[0].enter);
    }

    #[test]
    fn cursor_shape_parses() {
        for (text, want) in [
            ("bar", CursorShape::Bar),
            ("underline", CursorShape::Underline),
            ("block", CursorShape::Block),
            ("weird", CursorShape::Bar),
        ] {
            let cfg: Config = toml::from_str(&format!("[theme]\ncursor_shape = \"{text}\"\n")).unwrap();
            assert_eq!(cfg.theme.resolve().cursor_shape, want, "shape {text}");
        }
    }

    #[test]
    fn theme_bad_hex_falls_back() {
        let cfg: Config = toml::from_str("[theme]\nforeground = \"nope\"\n").unwrap();
        let colors = cfg.theme.resolve();
        assert_eq!(colors.fg, [0xdc, 0xdc, 0xdc]);
        assert_eq!(colors.cursor, None);
    }

    #[test]
    fn fx_resolve_honours_switch_and_values() {
        let off = FxConfig {
            enabled: false,
            ..FxConfig::default()
        };
        assert_eq!(off.resolve(FxKind::Fresh), None);
        let unknown = FxConfig {
            open: "spin".into(),
            ..FxConfig::default()
        };
        assert_eq!(unknown.resolve(FxKind::Fresh), None);
        let zero = FxConfig {
            close_ms: 0,
            ..FxConfig::default()
        };
        assert_eq!(zero.resolve(FxKind::Close), None);
        let big = FxConfig {
            focus_ms: 99999,
            ..FxConfig::default()
        };
        assert_eq!(big.resolve(FxKind::Focus), Some(("flash".into(), 2000)));
    }

    #[test]
    fn window_section_parses_and_clamps() {
        let cfg: Config =
            toml::from_str("[window]\nbackground_image = \"bg.png\"\nopacity = 0.4\ndecorations = false\n")
                .unwrap();
        assert_eq!(cfg.window.background_image, "bg.png");
        assert_eq!(cfg.window.clamped_opacity(), 0.4);
        assert!(!cfg.window.decorations);
        let odd = Window {
            opacity: 9.0,
            ..Window::default()
        };
        assert_eq!(odd.clamped_opacity(), 1.0);
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let cfg: Config = toml::from_str("[keys]\nquit = \"alt+q\"\n").unwrap();
        assert_eq!(cfg.keys.quit, vec!["alt+q".to_string()]);
        assert_eq!(cfg.keys.split_vertical, vec!["ctrl+v".to_string()]);
    }

    /// Configs compare by value: the window host uses this to notice
    /// hot-reloaded `[window]` edits (backdrop, decorations, transparency).
    #[test]
    fn config_equality_detects_edits() {
        let a = Config::default();
        assert_eq!(a, a.clone());
        let mut b = Config::default();
        b.window.background_image = "bg.png".into();
        assert_ne!(a, b);
        b.window.background_image.clear();
        assert_eq!(a, b);
        b.window.transparent = !b.window.transparent;
        assert_ne!(a, b);
    }

    /// The `[general]` key writer replaces in place, inserts after the
    /// header, or appends a section -- comments and siblings survive.
    #[test]
    fn set_general_key_edits_surgically() {
        // Replace existing, keep comments and other keys.
        let text = "# top\n[general]\n# keep me\nfont = \"old.ttf\"\nscrollback = 5\n[keys]\nquit = \"q\"\n";
        let out = set_general_key(text, "font", "\"new.ttf\"");
        assert!(out.contains("font = \"new.ttf\""), "out: {out}");
        assert!(!out.contains("old.ttf"), "out: {out}");
        assert!(out.contains("# keep me") && out.contains("scrollback = 5"));
        assert!(out.contains("[keys]"));
        // `fontx` is not `font`; commented lines are ignored.
        let tricky = "[general]\n# font = \"x\"\nfontx = 1\n";
        let out = set_general_key(tricky, "font", "\"n\"");
        assert!(out.contains("# font = \"x\"") && out.contains("fontx = 1"));
        assert!(out.contains("\nfont = \"n\""), "out: {out}");
        // Insert after the header when missing.
        let missing = "[general]\nscrollback = 5\n";
        assert_eq!(
            set_general_key(missing, "font", "\"n\""),
            "[general]\nfont = \"n\"\nscrollback = 5\n"
        );
        // Append a section when there is none; trailing newline preserved.
        assert_eq!(
            set_general_key("[keys]\nquit = \"q\"\n", "font", "\"n\""),
            "[keys]\nquit = \"q\"\n\n[general]\nfont = \"n\"\n"
        );
        assert_eq!(set_general_key("", "font", "\"n\""), "[general]\nfont = \"n\"");
    }

    /// `write_font_settings` round-trips through a real file: font + size
    /// written, file still parses, other content intact.
    #[test]
    fn write_font_settings_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "termrs-cfgfont-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "# comment\n[general]\nfont_size = 18\n").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut cfg = Config::parse_with_source(&text, Some(path.clone())).unwrap();
        cfg.general.font = "C:\\Fonts\\My Font.ttf".to_string();
        cfg.general.font_size = 30;
        assert_eq!(cfg.write_font_settings().unwrap(), path);
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(back.contains("# comment"), "comment kept: {back}");
        let reloaded: Config = toml::from_str(&back).unwrap();
        assert_eq!(reloaded.general.font, "C:\\Fonts\\My Font.ttf");
        assert_eq!(reloaded.general.font_size, 30);
        // An existing font_size line is replaced, not duplicated.
        assert_eq!(back.matches("font_size =").count(), 1, "back: {back}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
