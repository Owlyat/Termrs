//! App state: workspaces, inbox menu, prompt mode, key dispatch.

use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::{Input, Key as TaKey, TextArea};
use tachyonfx::EffectManager;
use throbber_widgets_tui::ThrobberState;
use tui_menu::{MenuEvent, MenuItem, MenuState};

use crate::commands_db::{self, CommandDb, SavedCommand};
use crate::config::{Config, FxKind, ThemeColors};use crate::font::FontEntry;
use crate::image_view::ImageView;
use crate::keys::{self, Key, KeyKind, KeyPress, Mods};
use crate::layout::LayoutFile;
use crate::scrollback::ScrollbackView;
use crate::workspace::{Direction, SplitDir, Workspace};

/// Prompt kinds for the two text-input overlays (textarea editors).
/// `kalk`, `lp` and `piper` are ordinary commands typed into a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    RenameWorkspace,
    ViewImage,
    AskAi,
    CheatSheet,
}

impl PromptKind {
    pub fn label(self) -> &'static str {
        match self {
            PromptKind::RenameWorkspace => "rename workspace>",
            PromptKind::ViewImage => "image path>",
            PromptKind::AskAi => "ask AI>",
            PromptKind::CheatSheet => "cheat.sh topic>",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            PromptKind::RenameWorkspace => "e.g. build",
            PromptKind::ViewImage => "e.g. screenshot.png",
            PromptKind::AskAi => "e.g. list the largest files here",
            PromptKind::CheatSheet => "e.g. tar",
        }
    }
}

/// Text input shown above the status bar.
/// Backed by `ratatui-textarea`: multi-line editing, history, Emacs keys.
pub struct Prompt {
    pub kind: PromptKind,
    pub editor: TextArea<'static>,
    /// Inline completion shown as dim "ghost" text; Tab accepts it.
    pub ghost: String,
}

impl Prompt {
    /// Fresh prompt with placeholder hint and reversed block cursor.
    pub fn open(kind: PromptKind) -> Self {
        let mut editor = TextArea::default();
        editor.set_placeholder_text(kind.hint());
        editor.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        Self {
            kind,
            editor,
            ghost: String::new(),
        }
    }

    /// Submitted text (multi-line joined).
    pub fn text(&self) -> String {
        self.editor.lines().join("\n")
    }
}

/// One-shot tachyonfx transition fired by ui once its target rect is known.
pub struct PendingFx {
    pub pane: usize,
    pub kind: FxKind,
}

/// A pane playing its close transition: still in the tree (so its rect is
/// known) until `until`, when the leaf is actually removed.
struct ClosingPane {
    ws: usize,
    id: usize,
    until: std::time::Instant,
}

/// Inbox menu actions (workspaces + status overview).
#[derive(Debug, Clone)]
pub enum WsAction {
    Switch(usize),
    New,
    CloseCurrent,
    RenameCurrent,
    /// Open the saved-command database picker.
    SavedCommands,
    /// Open the "save a new command" form.
    NewCommand,
    /// Import commands from a cheat.sh sheet (topic prompt, tick to save).
    ImportCheat,
}

/// What a mouse button press did (for status/tests; report bytes go
/// straight to the pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseClickOutcome {
    /// Nothing happened (disabled, modal overlay, no pane, no tracking).
    Ignored,
    /// A link was opened in the default browser.
    OpenedUrl,
    /// Report bytes were written to the pane.
    Forwarded,
    /// The click was consumed by the menu bar / dropdown (opened,
    /// switched, activated, or dismissed the menu). The caller must not
    /// focus a pane or forward bytes for this click.
    Menu,
}

/// Which overlay currently owns the keyboard (State pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Image,
    Scrollback,
    Select,
    Palette,
    CommandForm,
    CommandArgs,
    Commands,
    CheatPicker,
    FontPicker,
    Menu,
    Prompt,
    Normal,
}

/// Command palette entries (ctrl+p).
///
/// Variants intentionally repeat "Command" (`CommandPalette`, `SaveCommand`):
/// they read as palette actions, and renaming would churn every call site.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::enum_variant_names)]
pub enum Command {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    OpenMcpSplitH,
    OpenMcpSplitV,
    OpenMcpHere,
    ShareTerminal,
    StopSharing,
    ZoomPane,
    FocusNext,
    FocusPrev,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    NewWorkspace,
    CloseWorkspace,
    RenameWorkspace,
    WorkspaceNext,
    WorkspacePrev,
    Inbox,
    Scrollback,
    Image,
    Paste,
    CommandPalette,
    Commands,
    SaveCommand,
    SaveLayout,
    YankLast,
    SelectMode,
    EditConfig,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ChangeFont,
    AskAi,
    ImportCheat,
    Help,
    Quit,
}

impl Command {
    /// Every palette entry, with its label.
    const ALL: &'static [(Command, &'static str)] = &[
        (Command::SplitHorizontal, "Split horizontally"),
        (Command::SplitVertical, "Split vertically"),
        (Command::ClosePane, "Close pane"),
        (Command::OpenMcpSplitH, "Open MCP server splith"),
        (Command::OpenMcpSplitV, "Open MCP server splitv"),
        (Command::OpenMcpHere, "Open MCP server in current pane"),
        (Command::ShareTerminal, "Share terminal (iroh link)"),
        (Command::StopSharing, "Stop sharing this terminal"),
        (Command::ZoomPane, "Zoom pane (maximize)"),
        (Command::FocusNext, "Focus next pane"),
        (Command::FocusPrev, "Focus previous pane"),
        (Command::FocusLeft, "Focus pane left"),
        (Command::FocusRight, "Focus pane right"),
        (Command::FocusUp, "Focus pane up"),
        (Command::FocusDown, "Focus pane down"),
        (Command::ResizeLeft, "Resize pane left"),
        (Command::ResizeRight, "Resize pane right"),
        (Command::ResizeUp, "Resize pane up"),
        (Command::ResizeDown, "Resize pane down"),
        (Command::NewWorkspace, "New workspace"),
        (Command::CloseWorkspace, "Close workspace"),
        (Command::RenameWorkspace, "Rename workspace"),
        (Command::WorkspaceNext, "Next workspace"),
        (Command::WorkspacePrev, "Previous workspace"),
        (Command::Inbox, "Inbox (workspaces)"),
        (Command::Scrollback, "Scrollback viewer"),
        (Command::Image, "View image"),
        (Command::Paste, "Paste from clipboard"),
        (Command::CommandPalette, "Command palette"),
        (Command::Commands, "Saved commands (pick & run)"),
        (Command::SaveCommand, "Save command to database"),
        (Command::SaveLayout, "Save layout"),
        (Command::YankLast, "Yank last output"),
        (Command::SelectMode, "Select mode"),
        (Command::EditConfig, "Edit config & reload"),
        (Command::ZoomIn, "Zoom in"),
        (Command::ZoomOut, "Zoom out"),
        (Command::ZoomReset, "Zoom reset"),
        (Command::ChangeFont, "Change font..."),
        (Command::AskAi, "Ask AI for a command"),
        (Command::ImportCheat, "Import commands from cheat.sh"),
        (Command::Help, "Help"),
        (Command::Quit, "Quit"),
    ];
}

/// Visual selection inside the focused pane, in padded grid coordinates.
pub struct SelectMode {
    lines: Vec<String>,
    rows: u16,
    cols: u16,
    /// Anchor set by `v`/Enter; `None` until the user starts selecting.
    anchor: Option<(u16, u16)>,
    cur: (u16, u16),
    /// Pending multi-key prefix (`m` = match, `g` = goto).
    pending: Option<char>,
}

impl SelectMode {
    /// Snapshot the focused pane's visible grid and start at its cursor.
    fn enter(pane: &crate::pty::Pane) -> Self {
        let lines = pane.grid_lines();
        let rows = lines.len() as u16;
        let cols = lines.first().map(|l| l.chars().count()).unwrap_or(0) as u16;
        let (cr, cc) = pane.screen().cursor_position();
        let cur = (cr.min(rows.saturating_sub(1)), cc.min(cols.saturating_sub(1)));
        Self {
            lines,
            rows,
            cols,
            anchor: None,
            cur,
            pending: None,
        }
    }

    /// Move the cursor by a delta, clamped to the grid.
    fn move_by(&mut self, dx: i32, dy: i32) {
        let r = (self.cur.0 as i32 + dy).clamp(0, self.rows.saturating_sub(1) as i32) as u16;
        let c = (self.cur.1 as i32 + dx).clamp(0, self.cols.saturating_sub(1) as i32) as u16;
        self.cur = (r, c);
    }

    /// True when the cursor sits on the first visible row.
    fn at_top(&self) -> bool {
        self.cur.0 == 0
    }

    /// True when the cursor sits on the last visible row.
    fn at_bottom(&self) -> bool {
        self.rows == 0 || self.cur.0 + 1 >= self.rows
    }

    /// Swap in a fresh grid snapshot after the pane scrolled under the
    /// selection (`delta` = same sign as the pane scroll: +1 up into
    /// history, -1 down toward live). The cursor stays on its edge row (now
    /// showing new content); the anchor shifts with the content so the
    /// selection extends into history instead of jumping.
    fn refresh_after_scroll(&mut self, lines: Vec<String>, delta: i32) {
        self.rows = lines.len() as u16;
        self.cols = lines
            .first()
            .map(|l| l.chars().count())
            .unwrap_or(0) as u16;
        self.lines = lines;
        if self.rows == 0 {
            self.cur = (0, 0);
            self.anchor = None;
            return;
        }
        // Cursor stays on its row (caller keeps it at the edge), but the
        // column may need clamping when line widths change.
        self.cur.0 = self.cur.0.min(self.rows - 1);
        self.cur.1 = self.cur.1.min(self.cols.saturating_sub(1));
        // Old content shifted by `delta` rows in the new snapshot, so carry
        // the anchor along (clamped when it scrolled out of view).
        if let Some((ar, ac)) = self.anchor {
            let moved = (ar as i32 + delta).clamp(0, self.rows.saturating_sub(1) as i32) as u16;
            let clamped_c = ac.min(self.cols.saturating_sub(1));
            self.anchor = Some((moved, clamped_c));
        }
    }

    /// Set the anchor at the cursor if absent, clear it otherwise.
    fn toggle_anchor(&mut self) {
        self.anchor = if self.anchor.is_some() {
            None
        } else {
            Some(self.cur)
        };
    }

    /// Current cursor position in grid coordinates.
    pub fn cursor(&self) -> (u16, u16) {
        self.cur
    }

    /// Characters of row `r`, or an empty slice when out of range.
    fn line(&self, r: u16) -> Vec<char> {
        self.lines
            .get(r as usize)
            .map(|l| l.chars().collect())
            .unwrap_or_default()
    }

    /// Select from `start` to `end` (inclusive) on the cursor's row.
    fn set_selection(&mut self, start: u16, end: u16) {
        let row = self.cur.0;
        let last = self.cols.saturating_sub(1);
        let (s, e) = (start.min(last), end.min(last));
        self.anchor = Some((row, s));
        self.cur = (row, e);
    }

    /// Helix-style text object: `mi<obj>` (inner) / `ma<obj>` (around).
    fn select_object(&mut self, obj: char, inner: bool) {
        let row = self.cur.0;
        let col = self.cur.1;
        let chars = self.line(row);
        if chars.is_empty() {
            return;
        }
        match obj {
            'w' => {
                let (mut s, mut e) = (col as usize, col as usize);
                let at = chars.get(col as usize).copied();
                let prev = col.checked_sub(1).and_then(|c| chars.get(c as usize).copied());
                // Anchor on the word under the cursor, or the one just left.
                if at.map(is_word_char) != Some(true) && prev.map(is_word_char) == Some(true) {
                    s = col.saturating_sub(1) as usize;
                    e = s;
                } else if at.map(is_word_char) != Some(true) {
                    return;
                }
                while s > 0 && is_word_char(chars[s - 1]) {
                    s -= 1;
                }
                while e + 1 < chars.len() && is_word_char(chars[e + 1]) {
                    e += 1;
                }
                if !inner {
                    while e + 1 < chars.len() && chars[e + 1] == ' ' {
                        e += 1;
                    }
                }
                self.set_selection(s as u16, e as u16);
            }
            '"' | '\'' => {
                if let Some((s, e)) = pair_bounds(&chars, col, obj, obj) {
                    let (s, e) = if inner { (s + 1, e.saturating_sub(1)) } else { (s, e) };
                    self.set_selection(s, e);
                }
            }
            '(' | ')' | 'b' => self.pair_object(&chars, col, '(', ')', inner),
            '[' | ']' => self.pair_object(&chars, col, '[', ']', inner),
            '{' | '}' => self.pair_object(&chars, col, '{', '}', inner),
            _ => {}
        }
    }

    /// Helper for bracket-like pairs.
    fn pair_object(&mut self, chars: &[char], col: u16, open: char, close: char, inner: bool) {
        if let Some((s, e)) = pair_bounds(chars, col, open, close) {
            let (s, e) = if inner { (s + 1, e.saturating_sub(1)) } else { (s, e) };
            self.set_selection(s, e);
        }
    }

    /// Helix `g`-prefix motions. True when `c` was a known goto.
    fn goto(&mut self, c: char) -> bool {
        match c {
            'h' => self.cur = (self.cur.0, 0),
            // Last character on the line (last non-blank), not the padded end.
            'l' => {
                let line = self.line(self.cur.0);
                let last = line.iter().rposition(|c| !c.is_whitespace()).unwrap_or(0);
                self.cur = (self.cur.0, last as u16);
            }
            'g' => self.cur = (0, self.cur.1),
            'e' => self.cur = (self.rows.saturating_sub(1), self.cur.1),
            's' => {
                let row = self.cur.0;
                let first = self
                    .line(row)
                    .iter()
                    .position(|c| !c.is_whitespace())
                    .unwrap_or(0);
                self.cur = (row, first as u16);
            }
            _ => return false,
        }
        true
    }

    /// Swap the cursor to the other end of the selection (helix `Alt-;`).
    fn swap_ends(&mut self) {
        if let Some(a) = self.anchor.take() {
            self.anchor = Some(self.cur);
            self.cur = a;
        }
    }

    /// Move the cursor onto the next/previous `target` character on the line
    /// (`f`/`F`); extends the selection when one is active.
    fn find_char(&mut self, target: char, forward: bool) {
        let row = self.cur.0;
        let col = self.cur.1 as usize;
        let chars = self.line(row);
        let hit = if forward {
            chars
                .iter()
                .enumerate()
                .skip(col + 1)
                .find(|(_, c)| **c == target)
                .map(|(i, _)| i)
        } else {
            chars
                .iter()
                .enumerate()
                .take(col)
                .rev()
                .find(|(_, c)| **c == target)
                .map(|(i, _)| i)
        };
        if let Some(i) = hit {
            self.cur = (row, i as u16);
        }
    }

    /// Move till the next/previous `target` (helix `t`/`T`): stop one cell
    /// short, staying put when already adjacent.
    fn find_till(&mut self, target: char, forward: bool) {
        let row = self.cur.0;
        let col = self.cur.1 as usize;
        let chars = self.line(row);
        if forward {
            let hit = chars
                .iter()
                .enumerate()
                .skip(col + 1)
                .find(|(_, c)| **c == target)
                .map(|(i, _)| i);
            if let Some(i) = hit
                && i > col + 1 {
                    self.cur = (row, (i - 1) as u16);
                }
        } else {
            let hit = chars
                .iter()
                .enumerate()
                .take(col)
                .rev()
                .find(|(_, c)| **c == target)
                .map(|(i, _)| i);
            if let Some(i) = hit
                && i + 1 < col {
                    self.cur = (row, (i + 1) as u16);
                }
        }
    }

    /// Character at `(r, c)`; out-of-range reads as a space (the grid snapshot
    /// is padded, and padding selects as whitespace).
    fn at(&self, r: u16, c: u16) -> char {
        self.lines
            .get(r as usize)
            .and_then(|l| l.chars().nth(c as usize))
            .unwrap_or(' ')
    }

    /// Character count of row `r` (grid rows may be ragged in tests; the
    /// live grid snapshot is padded to a uniform width).
    fn line_len(&self, r: u16) -> usize {
        self.lines
            .get(r as usize)
            .map(|l| l.chars().count())
            .unwrap_or(0)
    }

    /// One step forward in reading order, stopping at each line's real end
    /// (trailing padding is skipped transparently); `None` past the end.
    fn step_fwd(&self, r: u16, c: u16) -> Option<(u16, u16)> {
        if self.rows == 0 {
            return None;
        }
        if (c as usize) + 1 < self.line_len(r) {
            Some((r, c + 1))
        } else if r + 1 < self.rows {
            Some((r + 1, 0))
        } else {
            None
        }
    }

    /// One step back in reading order, landing on the previous line's last
    /// real cell; `None` before the first cell.
    fn step_back(&self, r: u16, c: u16) -> Option<(u16, u16)> {
        if self.rows == 0 {
            return None;
        }
        if c > 0 {
            Some((r, c - 1))
        } else if r > 0 {
            Some((r - 1, self.line_len(r - 1).saturating_sub(1) as u16))
        } else {
            None
        }
    }

    /// Word test: `w`/`b`/`e` stop at alphanumerics + `_`, `W`/`B`/`E`
    /// (big words) at any non-whitespace run.
    fn word_char(c: char, big: bool) -> bool {
        if big {
            !c.is_whitespace()
        } else {
            is_word_char(c)
        }
    }

    /// Helix `w`/`W`: move to the next word start, crossing lines.
    fn word_forward(&mut self, big: bool) {
        let (mut r, mut c) = self.cur;
        // Skip the rest of the word under the cursor first.
        if Self::word_char(self.at(r, c), big) {
            while let Some((nr, nc)) = self.step_fwd(r, c) {
                if !Self::word_char(self.at(nr, nc), big) {
                    break;
                }
                (r, c) = (nr, nc);
            }
        }
        // Skip whitespace to the next word start.
        while let Some((nr, nc)) = self.step_fwd(r, c) {
            (r, c) = (nr, nc);
            if Self::word_char(self.at(nr, nc), big) {
                break;
            }
        }
        self.cur = (r, c);
    }

    /// Helix `b`/`B`: move to the previous word start, crossing lines.
    fn word_backward(&mut self, big: bool) {
        let Some(mut p) = self.step_back(self.cur.0, self.cur.1) else {
            return; // Already at the first cell: stay.
        };
        // Skip whitespace backwards to the previous word...
        loop {
            if Self::word_char(self.at(p.0, p.1), big) {
                break;
            }
            match self.step_back(p.0, p.1) {
                Some(q) => p = q,
                None => {
                    self.cur = (0, 0);
                    return;
                }
            }
        }
        // ...then back to its start.
        let mut start = p;
        while let Some(q) = self.step_back(start.0, start.1) {
            if !Self::word_char(self.at(q.0, q.1), big) {
                break;
            }
            start = q;
        }
        self.cur = start;
    }

    /// Helix `e`/`E`: move to the next word end, crossing lines.
    fn word_end(&mut self, big: bool) {
        let Some((mut r, mut c)) = self.step_fwd(self.cur.0, self.cur.1) else {
            return; // Already at the last cell: stay.
        };
        // Skip whitespace forwards to the next word...
        loop {
            if Self::word_char(self.at(r, c), big) {
                break;
            }
            match self.step_fwd(r, c) {
                Some((nr, nc)) => {
                    (r, c) = (nr, nc);
                }
                None => {
                    self.cur = (r, c);
                    return;
                }
            }
        }
        // ...then forward to its end.
        while let Some((nr, nc)) = self.step_fwd(r, c) {
            if !Self::word_char(self.at(nr, nc), big) {
                break;
            }
            (r, c) = (nr, nc);
        }
        self.cur = (r, c);
    }

    /// Last non-blank column of `row` (0 for blank rows), clamped to the grid.
    fn last_nonblank(&self, row: u16) -> u16 {
        self.line(row)
            .iter()
            .rposition(|c| !c.is_whitespace())
            .map(|i| (i as u16).min(self.cols.saturating_sub(1)))
            .unwrap_or(0)
    }

    /// Helix `x`: select the cursor's line; when it is already fully
    /// selected, extend through the end of the next line instead.
    fn extend_line(&mut self) {
        let row = self.cur.0;
        let last = self.last_nonblank(row);
        let full = self.anchor == Some((row, 0)) && self.cur == (row, last);
        if full && row + 1 < self.rows {
            self.cur = (row + 1, self.last_nonblank(row + 1));
        } else {
            self.anchor = Some((row, 0));
            self.cur = (row, last);
        }
    }

    /// Helix `X`: extend the selection to the cursor line's bounds.
    fn extend_line_bounds(&mut self) {
        let row = self.cur.0;
        self.anchor = Some((row, 0));
        self.cur = (row, self.last_nonblank(row));
    }

    /// Helix `%`: select the whole grid.
    fn select_all(&mut self) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        self.anchor = Some((0, 0));
        self.cur = (self.rows - 1, self.cols - 1);
    }

    /// Helix `;`: collapse the selection onto the cursor (zero-width).
    fn collapse(&mut self) {
        self.anchor = Some(self.cur);
    }

    /// Helix `mm`: jump to the bracket matching the one under the cursor
    /// (`()[]{}` on the same line, nesting-aware). No match: stay put.
    fn match_bracket(&mut self) {
        const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
        let row = self.cur.0;
        let col = self.cur.1 as usize;
        let chars = self.line(row);
        let Some(ch) = chars.get(col).copied() else {
            return;
        };
        for (open, close) in PAIRS {
            if ch == open {
                let mut depth = 0u32;
                for (j, cc) in chars.iter().enumerate().skip(col) {
                    if *cc == open {
                        depth += 1;
                    } else if *cc == close {
                        depth -= 1;
                        if depth == 0 {
                            self.cur = (row, j as u16);
                            return;
                        }
                    }
                }
                return;
            } else if ch == close {
                let mut depth = 0u32;
                for (j, cc) in chars.iter().enumerate().take(col + 1).rev() {
                    if *cc == close {
                        depth += 1;
                    } else if *cc == open {
                        depth -= 1;
                        if depth == 0 {
                            self.cur = (row, j as u16);
                            return;
                        }
                    }
                }
                return;
            }
        }
    }

    /// Build a selection from literal lines (tests, no pane needed).
    #[cfg(test)]
    fn from_lines(lines: Vec<String>, cur: (u16, u16)) -> Self {
        let rows = lines.len() as u16;
        let cols = lines.first().map(|l| l.chars().count()).unwrap_or(0) as u16;
        Self {
            lines,
            rows,
            cols,
            anchor: None,
            cur,
            pending: None,
        }
    }

    /// Normalized selection `(r0, c0, r1, c1)`, inclusive, or `None`.
    pub fn range(&self) -> Option<(u16, u16, u16, u16)> {
        let (ar, ac) = self.anchor?;
        let (br, bc) = self.cur;
        let (r0, r1, c0, c1) = if (ar, ac) <= (br, bc) {
            (ar, br, ac, bc)
        } else {
            (br, ar, bc, ac)
        };
        Some((r0, c0, r1, c1))
    }

    /// Selected text (with `\n` between rows) or the whole grid without an
    /// anchor, trimmed.
    fn selected_text(&self) -> String {
        let text = match self.range() {
            // Whole grid (no anchor): trim each row's padding.
            None => self
                .lines
                .iter()
                .map(|l| l.trim_end())
                .collect::<Vec<_>>()
                .join("\n"),
            Some((r0, c0, r1, c1)) => {
                let mut out = String::new();
                for r in r0..=r1 {
                    let Some(line) = self.lines.get(r as usize) else {
                        continue;
                    };
                    let chars: Vec<char> = line.chars().collect();
                    let last = chars.len().saturating_sub(1) as u16;
                    // Block selection: middle rows run edge to edge.
                    let (start, end) = if r0 == r1 {
                        (c0, c1.min(last))
                    } else if r == r0 {
                        (c0, last)
                    } else if r == r1 {
                        (0, c1.min(last))
                    } else {
                        (0, last)
                    };
                    let slice: String = if start <= end {
                        chars[start as usize..=end as usize].iter().collect()
                    } else {
                        String::new()
                    };
                    if r > r0 {
                        out.push('\n');
                    }
                    out.push_str(slice.trim_end());
                }
                out
            }
        };
        text.trim_matches('\n').trim_end().to_string()
    }
}

/// Build the shell command that opens `path` in `editor`, quoting for the
/// platform shell so paths with spaces work.
fn edit_invocation(editor: &str, path: &std::path::Path) -> String {
    let shown = path.display();
    if cfg!(windows) {
        format!("{editor} \"{shown}\"")
    } else {
        format!("{editor} '{shown}'")
    }
}

/// Word character for text objects (alphanumeric or underscore).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True for a Ctrl+C keypress (any letter case, Shift tolerated): the shell's
/// interrupt key. Alt/Super combos are different bindings and must not match.
fn is_interrupt(ev: &KeyPress) -> bool {
    if !ev.mods.contains(Mods::CONTROL) {
        return false;
    }
    if ev.mods.contains(Mods::ALT) || ev.mods.contains(Mods::SUPER) {
        return false;
    }
    matches!(ev.key, Key::Char('c') | Key::Char('C'))
}

/// Nearest `open`..`close` pair surrounding column `col` on one line.
fn pair_bounds(chars: &[char], col: u16, open: char, close: char) -> Option<(u16, u16)> {
    let c = col as usize;
    let s = (0..=c.min(chars.len().saturating_sub(1)))
        .rev()
        .find(|&i| chars[i] == open)?;
    let e = ((c.min(chars.len().saturating_sub(1)))..chars.len()).find(|&i| chars[i] == close)?;
    if s > e {
        return None;
    }
    Some((s as u16, e as u16))
}

/// One command-palette entry after fuzzy filtering.
pub struct PaletteItem {
    pub cmd: Command,
    pub label: &'static str,
    /// Indices in `label` matched by the query (for highlighting).
    pub indices: Vec<usize>,
}

/// Command palette state: a fuzzy query plus its ranked results.
pub struct Palette {
    pub query: String,
    pub results: Vec<PaletteItem>,
    pub selected: usize,
}

impl Palette {
    /// Fresh palette: empty query, every command, first selected.
    fn new() -> Self {
        let mut p = Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Recompute results from the query (fuzzy match + score order).
    fn refilter(&mut self) {
        use fuzzy_matcher::FuzzyMatcher;
        use fuzzy_matcher::skim::SkimMatcherV2;
        let matcher = SkimMatcherV2::default();
        let mut results: Vec<(i64, PaletteItem)> = if self.query.trim().is_empty() {
            Command::ALL
                .iter()
                .map(|(cmd, label)| {
                    (
                        0,
                        PaletteItem {
                            cmd: *cmd,
                            label,
                            indices: Vec::new(),
                        },
                    )
                })
                .collect()
        } else {
            Command::ALL
                .iter()
                .filter_map(|(cmd, label)| {
                    matcher
                        .fuzzy_indices(label, &self.query)
                        .map(|(score, indices)| {
                            (
                                score,
                                PaletteItem {
                                    cmd: *cmd,
                                    label,
                                    indices,
                                },
                            )
                        })
                })
                .collect()
        };
        // Best score first; stable for equal scores (keeps ALL order).
        results.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.results = results.into_iter().map(|(_, item)| item).collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    /// Move the selection by `delta`, wrapping.
    fn move_sel(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
    }

    /// Add a typed character to the query.
    fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.refilter();
    }

    /// Delete the last query character.
    fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.selected = 0;
            self.refilter();
        }
    }
}
/// One saved-command row after filtering, with highlight indices.
pub struct CommandHit {
    pub item: SavedCommand,
    /// Indices in `item.command` matched by the query.
    pub indices: Vec<usize>,
}

/// Saved-command picker overlay (ctrl+r): a fuzzy query over the database.
pub struct CommandPicker {
    pub query: String,
    all: Vec<SavedCommand>,
    pub results: Vec<CommandHit>,
    pub selected: usize,
}

impl CommandPicker {
    fn new(all: Vec<SavedCommand>) -> Self {
        let mut p = Self {
            query: String::new(),
            all,
            results: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        use fuzzy_matcher::FuzzyMatcher;
        use fuzzy_matcher::skim::SkimMatcherV2;
        let matcher = SkimMatcherV2::default();
        let q = self.query.trim();
        let mut hits: Vec<(i64, CommandHit)> = if q.is_empty() {
            self.all
                .iter()
                .map(|c| {
                    (
                        0,
                        CommandHit {
                            item: c.clone(),
                            indices: Vec::new(),
                        },
                    )
                })
                .collect()
        } else {
            self.all
                .iter()
                .filter_map(|c| {
                    let hay = format!("{} {}", c.command, c.comment);
                    matcher.fuzzy_indices(&hay, q).map(|(score, _)| {
                        let indices = matcher
                            .fuzzy_indices(&c.command, q)
                            .map(|(_, i)| i)
                            .unwrap_or_default();
                        (score, CommandHit { item: c.clone(), indices })
                    })
                })
                .collect()
        };
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.results = hits.into_iter().map(|(_, h)| h).collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    fn move_sel(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
    }

    fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.refilter();
    }

    fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.selected = 0;
            self.refilter();
        }
    }

    fn remove_id(&mut self, id: i64) {
        self.all.retain(|c| c.id != id);
        self.selected = 0;
        self.refilter();
    }
}

/// One fetched cheat.sh row plus its tick box.
pub struct CheatPickItem {
    pub entry: crate::cheatsheet::CheatEntry,
    pub checked: bool,
}

/// One import-picker row after filtering: index into the items plus
/// highlight indices in the command.
pub struct CheatHit {
    pub index: usize,
    /// Indices in the command matched by the query (for highlighting).
    pub indices: Vec<usize>,
}

/// Import review overlay: fetched rows start unticked; space ticks the
/// highlighted row, ctrl+a ticks all shown rows, ctrl+u clears them, Enter
/// adds the ticked rows, ctrl+e edits the highlighted row, Esc cancels.
pub struct CheatPicker {
    pub topic: String,
    pub query: String,
    pub items: Vec<CheatPickItem>,
    pub results: Vec<CheatHit>,
    pub selected: usize,
}

impl CheatPicker {
    fn new(topic: String, entries: Vec<crate::cheatsheet::CheatEntry>) -> Self {
        let mut p = Self {
            topic,
            query: String::new(),
            items: entries
                .into_iter()
                .map(|entry| CheatPickItem { entry, checked: false })
                .collect(),
            results: Vec::new(),
            selected: 0,
        };
        p.refilter();
        p
    }

    /// Borrow the item behind a filtered hit.
    pub fn item(&self, hit: &CheatHit) -> Option<&CheatPickItem> {
        self.items.get(hit.index)
    }

    fn refilter(&mut self) {
        use fuzzy_matcher::FuzzyMatcher;
        use fuzzy_matcher::skim::SkimMatcherV2;
        let matcher = SkimMatcherV2::default();
        let q = self.query.trim();
        let mut hits: Vec<(i64, CheatHit)> = if q.is_empty() {
            self.items
                .iter()
                .enumerate()
                .map(|(index, _)| (0, CheatHit { index, indices: Vec::new() }))
                .collect()
        } else {
            self.items
                .iter()
                .enumerate()
                .filter_map(|(index, item)| {
                    let hay = format!("{} {}", item.entry.command, item.entry.comment);
                    matcher.fuzzy_indices(&hay, q).map(|(score, _)| {
                        let indices = matcher
                            .fuzzy_indices(&item.entry.command, q)
                            .map(|(_, i)| i)
                            .unwrap_or_default();
                        (score, CheatHit { index, indices })
                    })
                })
                .collect()
        };
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.results = hits.into_iter().map(|(_, h)| h).collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    fn move_sel(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as i32;
        self.selected = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
    }

    fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.refilter();
    }

    fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.selected = 0;
            self.refilter();
        }
    }

    /// Tick/untick the highlighted row.
    fn toggle_selected(&mut self) {
        if let Some(hit) = self.results.get(self.selected)
            && let Some(item) = self.items.get_mut(hit.index) {
                item.checked = !item.checked;
            }
    }

    /// Tick (`on`) or clear (`!on`) every currently shown row.
    fn select_visible(&mut self, on: bool) {
        for hit in &self.results {
            if let Some(item) = self.items.get_mut(hit.index) {
                item.checked = on;
            }
        }
    }

    /// Number of ticked rows (shown in the title).
    pub fn checked_count(&self) -> usize {
        self.items.iter().filter(|i| i.checked).count()
    }

    /// Cloned ticked entries, in sheet order.
    fn checked_entries(&self) -> Vec<crate::cheatsheet::CheatEntry> {
        self.items
            .iter()
            .filter(|i| i.checked)
            .map(|i| i.entry.clone())
            .collect()
    }
}

/// A finished cheat.sh import: the topic plus its parsed rows, delivered
/// from the background fetch thread.
struct CheatImport {
    topic: String,
    entries: Vec<crate::cheatsheet::CheatEntry>,
}

/// One installed font after filtering, with highlight indices.
pub struct FontHit {
    pub item: FontEntry,
    /// Indices in `item.name` matched by the query.
    pub indices: Vec<usize>,
    /// Monospace probe result (`None` = unknown); `Some(false)` rows get a
    /// warning tag since proportional fonts misalign the terminal grid.
    pub mono: Option<bool>,
}

/// Font picker overlay: a fuzzy query over installed fonts. The first entry
/// is always "System default (auto-detect)" (empty path).
pub struct FontPicker {
    pub query: String,
    all: Vec<FontEntry>,
    pub results: Vec<FontHit>,
    pub selected: usize,
    /// Currently active font path, to mark it in the list.
    pub active: String,
    /// Which field owns the keyboard: the font list or the size box.
    pub focus: PickerFocus,
    /// Preview glyph height, live-applied to the terminal while browsing.
    pub size: u32,
    /// Digit buffer while typing a size (cleared on focus/adjust).
    size_buf: Option<String>,
    /// Live state at open, restored when the picker is cancelled.
    orig_font: String,
    orig_size: u32,
}

/// One font-preview render job for the background worker. Only the latest
/// generation's result is ever shown; older ones are dropped on arrival.
struct PreviewRequest {
    generation: u64,
    path: PathBuf,
    name: String,
    title_px: f32,
    fg: [u8; 3],
    bg: [u8; 3],
}

/// A finished preview render: generation + the key it was rendered for.
struct PreviewOutcome {
    generation: u64,
    path: String,
    size: u32,
    result: Result<crate::font::PreviewPixels, String>,
}

/// Sample line under the font name in picker previews.
const PREVIEW_BODY: &str = "The quick brown fox 0123456789 !@#$%";

/// Keyboard focus inside the font picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerFocus {
    List,
    Size,
}

/// CPU-rasterized preview of the highlighted font (shown through the image
/// overlay while the picker is open).
pub struct FontPreview {
    /// Cache key: (font path, title px).
    pub key: (String, u32),
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl FontPicker {
    fn new(mut all: Vec<FontEntry>, active: &str, size: u32) -> Self {
        all.insert(
            0,
            FontEntry {
                name: "System default (auto-detect)".to_string(),
                path: PathBuf::new(),
                mono: None,
            },
        );
        let mut p = Self {
            query: String::new(),
            all,
            results: Vec::new(),
            selected: 0,
            active: active.to_string(),
            focus: PickerFocus::List,
            size: size.clamp(6, 48),
            size_buf: None,
            orig_font: active.to_string(),
            orig_size: size.clamp(6, 48),
        };
        p.refilter();
        // Preselect the active font (when it is a real file) so its preview
        // shows immediately; auto-detect keeps the "system default" row.
        if !p.active.trim().is_empty()
            && let Some(i) = p.results.iter().position(|h| {
                !h.item.path.as_os_str().is_empty()
                    && h.item.path.to_string_lossy() == p.active
            }) {
                p.selected = i;
            }
        p
    }

    fn refilter(&mut self) {
        use fuzzy_matcher::FuzzyMatcher;
        use fuzzy_matcher::skim::SkimMatcherV2;
        let matcher = SkimMatcherV2::default();
        let q = self.query.trim();
        let mut hits: Vec<(i64, FontHit)> = if q.is_empty() {
            self.all
                .iter()
                .map(|f| {
                    (
                        0,
                        FontHit {
                            item: f.clone(),
                            indices: Vec::new(),
                            mono: f.mono,
                        },
                    )
                })
                .collect()
        } else {
            self.all
                .iter()
                .filter_map(|f| {
                    matcher.fuzzy_indices(&f.name, q).map(|(score, indices)| {
                        (
                            score,
                            FontHit {
                                item: f.clone(),
                                indices,
                                mono: f.mono,
                            },
                        )
                    })
                })
                .collect()
        };
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.results = hits.into_iter().map(|(_, h)| h).collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    fn move_sel(&mut self, delta: i32) {
        if self.results.is_empty() {
            return;
        }
        let n = self.results.len() as i32;
        let next = ((self.selected as i32 + delta).rem_euclid(n)) as usize;
        if next != self.selected {
            self.selected = next;
        }
    }

    fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.refilter();
    }

    fn backspace(&mut self) {
        if self.query.pop().is_some() {
            self.selected = 0;
            self.refilter();
        }
    }

    /// Step the preview size (live-applied by the caller).
    fn nudge_size(&mut self, delta: i32) {
        let next = (self.size as i32 + delta).clamp(6, 48) as u32;
        if next != self.size {
            self.size = next;
            self.size_buf = None;
        }
    }

    /// Text for the size box: the live digits with a caret while focused,
    /// else the committed value.
    pub fn size_display(&self, focused: bool) -> String {
        match (&self.size_buf, focused) {
            (Some(buf), true) => format!("{buf}_"),
            _ => format!("{}", self.size),
        }
    }

    /// Type a size digit: first digit after focus/adjust replaces, the rest
    /// append (max two digits); out-of-range extras are ignored.
    fn push_digit(&mut self, d: char) {
        debug_assert!(d.is_ascii_digit());
        let mut buf = self.size_buf.take().unwrap_or_default();
        if buf.len() >= 2 {
            buf.clear();
        }
        buf.push(d);
        if let Ok(v) = buf.parse::<u32>() {
            let next = v.clamp(6, 48);
            self.size_buf = Some(buf);
            if next != self.size {
                self.size = next;
            }
        } else {
            self.size_buf = Some(buf);
        }
    }

    /// Backspace in the size box: drop a digit, or reset to the entry size
    /// when the buffer runs out.
    fn size_backspace(&mut self) {
        let mut buf = self.size_buf.take().unwrap_or_default();
        buf.pop();
        if buf.is_empty() {
            self.size_buf = None;
            if self.size != self.orig_size {
                self.size = self.orig_size;
            }
        } else if let Ok(v) = buf.parse::<u32>() {
            self.size_buf = Some(buf);
            let next = v.clamp(6, 48);
            if next != self.size {
                self.size = next;
            }
        } else {
            self.size_buf = Some(buf);
        }
    }

    /// Cache a monospace probe result on the matching entry. Entries (and
    /// their flags) survive refilters, so each font is probed at most once.
    fn cache_mono(&mut self, path: &std::path::Path, mono: bool) {
        if let Some(e) = self.all.iter_mut().find(|e| e.path == path) {
            e.mono = Some(mono);
        }
    }

    /// Probe monospace flags for results `[start, start+count)`. Called by
    /// the renderer before drawing rows: visible rows only, one parse per
    /// font ever (cached on the entry).
    pub(crate) fn probe_visible(&mut self, start: usize, count: usize) {
        let paths: Vec<std::path::PathBuf> = self
            .results
            .iter()
            .skip(start)
            .take(count)
            .filter(|h| h.mono.is_none() && !h.item.path.as_os_str().is_empty())
            .map(|h| h.item.path.clone())
            .collect();
        for path in paths {
            if let Ok(mono) = crate::font::is_monospaced(&path) {
                self.cache_mono(&path, mono);
            }
        }
        for hit in self.results.iter_mut().skip(start).take(count) {
            if hit.mono.is_none() {
                hit.mono = self
                    .all
                    .iter()
                    .find(|e| e.path == hit.item.path)
                    .and_then(|e| e.mono);
            }
        }
    }
}

/// Where the command form was opened from (controls what Enter writes and
/// where Esc returns).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormOrigin {
    /// Fresh save (ctrl+shift+r): Enter inserts into the database.
    New,
    /// Opened from the saved-command picker with ctrl+e:
    /// Enter updates the DB row, Esc returns to the picker.
    CommandPicker,
    /// Opened from the cheat.sh import picker with ctrl+e:
    /// Enter rewrites the pending row, Esc returns to the import picker.
    CheatPicker,
}

/// Two-field edit table for saving or editing a command (command text +
/// comment). Rendered as two labelled rows (Tab switches field, Enter
/// validates, Esc goes back).
pub struct CommandForm {
    pub command: TextArea<'static>,
    pub comment: TextArea<'static>,
    /// 0 = command, 1 = comment.
    pub field: usize,
    /// `Some(id)` when editing a saved DB row (Enter updates it in place).
    pub edit_id: Option<i64>,
    /// `Some(index)` when editing a pending cheat.sh row (Enter rewrites it
    /// in the import picker).
    pub edit_cheat: Option<usize>,
    /// Where this form came from.
    pub origin: FormOrigin,
}

impl CommandForm {
    fn new() -> Self {
        let mut command = TextArea::default();
        command.set_placeholder_text("e.g. git log --oneline -n {count}");
        command.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut comment = TextArea::default();
        comment.set_placeholder_text("what does it do?");
        comment.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
        Self {
            command,
            comment,
            field: 0,
            edit_id: None,
            edit_cheat: None,
            origin: FormOrigin::New,
        }
    }

    /// Edit table prefilled from a saved row (tags and use counter are kept
    /// by the update path). Esc returns to the command picker.
    fn edit_command(item: &SavedCommand) -> Self {
        let mut form = Self::new();
        if !item.command.is_empty() {
            form.command.insert_str(&item.command);
        }
        if !item.comment.is_empty() {
            form.comment.insert_str(&item.comment);
        }
        form.edit_id = Some(item.id);
        form.origin = FormOrigin::CommandPicker;
        form
    }

    /// Edit table prefilled from a pending cheat.sh row at `index`. Enter
    /// rewrites that row in place; Esc returns to the import picker.
    fn edit_cheat(index: usize, entry: &crate::cheatsheet::CheatEntry) -> Self {
        let mut form = Self::new();
        if !entry.command.is_empty() {
            form.command.insert_str(&entry.command);
        }
        if !entry.comment.is_empty() {
            form.comment.insert_str(&entry.comment);
        }
        form.edit_cheat = Some(index);
        form.origin = FormOrigin::CheatPicker;
        form
    }

    pub fn command_text(&self) -> String {
        self.command.lines().join(" ").trim().to_string()
    }

    pub fn comment_text(&self) -> String {
        self.comment.lines().join(" ").trim().to_string()
    }

    fn active_mut(&mut self) -> &mut TextArea<'static> {
        if self.field == 0 {
            &mut self.command
        } else {
            &mut self.comment
        }
    }
}

/// One input per `{name}` placeholder before a saved command runs.
pub struct CommandArgs {
    pub id: i64,
    pub template: String,
    pub names: Vec<String>,
    pub fields: Vec<TextArea<'static>>,
    pub index: usize,
}

impl CommandArgs {
    fn new(id: i64, template: String) -> Self {
        let names = commands_db::placeholders(&template);
        let fields = names
            .iter()
            .map(|_| {
                let mut t = TextArea::default();
                t.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
                t
            })
            .collect();
        Self {
            id,
            template,
            names,
            fields,
            index: 0,
        }
    }

    fn values(&self) -> Vec<(String, String)> {
        self.names
            .iter()
            .cloned()
            .zip(self.fields.iter().map(|f| f.lines().join(" ").trim().to_string()))
            .collect()
    }
}

/// Full app state.
pub struct App {
    pub config: Config,
    /// Shared braille dot list: the UI fills it each frame and the GPU
    /// post-processor draws it. `window` hands the same `Arc` to both.
    pub braille: std::sync::Arc<std::sync::Mutex<Vec<crate::image_gpu::Dot>>>,
    workspaces: Vec<Workspace>,
    current: usize,
    prompt: Option<Prompt>,
    /// Inbox menu state; the menubar renders every frame, the dropdown only
    /// while `menu_open` routes keys to it.
    menu: MenuState<WsAction>,
    menu_open: bool,
    /// Command palette overlay (ctrl+p).
    palette: Option<Palette>,
    /// SQLite database of saved commands (ctrl+r picker, ctrl+shift+r save).
    db: Option<CommandDb>,
    db_path: PathBuf,
    command_picker: Option<CommandPicker>,
    command_form: Option<CommandForm>,
    command_args: Option<CommandArgs>,
    /// cheat.sh import review overlay (tick rows to save).
    cheat_picker: Option<CheatPicker>,
    /// Font picker overlay (palette "Change font...").
    font_picker: Option<FontPicker>,
    /// CPU-rasterized preview of the highlighted font, shown through the
    /// image overlay while the picker is open.
    font_preview: Option<FontPreview>,
    /// Background preview renderer: requests in, finished rasters out.
    /// Dropped (killing the worker) when the picker closes.
    font_preview_tx: Option<crossbeam_channel::Sender<PreviewRequest>>,
    font_preview_rx: Option<crossbeam_channel::Receiver<PreviewOutcome>>,
    /// Monotonic generation: stale worker results never reach the screen.
    font_preview_gen: u64,
    /// A preview render is in flight (spinner shows in the preview box).
    font_preview_loading: bool,
    /// Last requested (path, size): repeats are not re-sent to the worker.
    font_preview_sent: Option<(String, u32)>,
    /// Pending primary-font change for the window host to apply: the new
    /// `[general] font` value (empty = auto-detect).
    font_request: Option<String>,
    /// Config file mtime at last load, for hot reload.
    config_mtime: Option<std::time::SystemTime>,
    /// Frame counter for throttled hot-reload checks.
    reload_tick: u32,
    /// cwd generation last used to build ghost completions; when the shell
    /// replies with a fresher cwd, completions are rebuilt.
    cwd_seq_seen: u64,
    /// Pending AI answer (runs on a background thread).
    ai_rx: Option<crossbeam_channel::Receiver<Result<String, String>>>,
    /// Pending cheat.sh import (runs on a background thread).
    cheat_rx: Option<crossbeam_channel::Receiver<Result<CheatImport, String>>>,
    scrollback: Option<ScrollbackView>,
    /// Visual selection overlay for the focused pane.
    select: Option<SelectMode>,
    /// Open image overlay.
    image: Option<ImageView>,
    /// Aspect-fitted popup cell rect for the image overlay, computed by the
    /// window host (which owns pixel metrics) just before each draw. The ui
    /// border uses it so chrome and texture line up exactly.
    image_rect: Option<Rect>,
    /// Last pointer report forwarded to a pane: `(pane id, inner col, inner
    /// row, left held)`. Motion is only forwarded when this changes, so a
    /// held/jittery pointer does not flood the app with identical reports
    /// (and holding still sends nothing until release).
    last_mouse_cell: Option<(usize, u16, u16, bool)>,
    /// Big-text help overlay.
    pub about: bool,
    pub status: String,
    pub should_quit: bool,
    /// Spinner state for the status-bar throbber.
    pub throbber: ThrobberState,
    /// Active tachyonfx transitions.
    pub effects: EffectManager<&'static str>,
    pending_fx: Vec<PendingFx>,
    /// Panes playing their close transition (removed at each deadline).
    closing: Vec<ClosingPane>,
    last_frame: std::time::Instant,
    /// Where the pane layout is saved between runs.
    layout_path: PathBuf,
    /// Tokio handle: pane readers and the input worker run on it.
    rt: tokio::runtime::Handle,
    /// Resolved theme palette (parsed once, no per-frame warnings).
    theme: ThemeColors,
    /// Nudges the UI thread whenever any pane delivers output.
    wake: crossbeam_channel::Sender<()>,
    /// Live glyph height in pixels (zoomable via ctrl+plus/minus/0).
    font_size: u32,
    /// Pending font-size change for the window host to apply.
    zoom_request: Option<u32>,
    /// IPC server: commands in, responses out.
    ipc_cmd_rx: Option<crossbeam_channel::Receiver<crate::ipc::IpcCommand>>,
    ipc_resp_tx: Option<crossbeam_channel::Sender<crate::ipc::Response>>,
    /// HTTP server: commands in, status/screenshot out.
    http_cmd_rx: Option<crossbeam_channel::Receiver<crate::server::ServerCommand>>,
    http_status_tx: Option<crossbeam_channel::Sender<String>>,
    http_screenshot_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    /// Pane-scoped MCP servers, keyed by pane id (see `crate::mcp`).
    /// A pane's server is stopped when the pane disappears.
    mcp_servers: std::collections::HashMap<usize, crate::mcp::PaneMcp>,
    /// Tool calls from MCP servers land here; the UI thread answers them in
    /// [`App::poll_mcp`].
    mcp_query_tx: crossbeam_channel::Sender<crate::mcp::McpQuery>,
    mcp_query_rx: Option<crossbeam_channel::Receiver<crate::mcp::McpQuery>>,
    /// iroh share sessions keyed by pane id (see `crate::share`).
    share_sessions: std::collections::HashMap<usize, crate::share::ShareSession>,
    /// Snapshot/input requests from share viewers land here; the UI thread
    /// answers them in [`App::poll_share`].
    share_query_tx: crossbeam_channel::Sender<crate::share::ShareQuery>,
    share_query_rx: Option<crossbeam_channel::Receiver<crate::share::ShareQuery>>,
}

impl App {
    /// Boot from the saved layout when present, else one "main" workspace.
    /// `layout_override` (from `--layout`) wins over the config-adjacent path.
    /// `rt` hosts one blocking reader thread per pane; `wake` nudges the UI
    /// thread whenever any pane delivers output.
    pub fn new(
        config: Config,
        layout_override: Option<PathBuf>,
        rt: &tokio::runtime::Handle,
        wake: &crossbeam_channel::Sender<()>,
        ipc: Option<crate::ipc::IpcServer>,
        http_server: Option<crate::server::HttpServerHandle>,
        server_mode: bool,
    ) -> Result<Self, String> {
        // Server mode (--server) is an isolated remote-control session: it
        // must neither load nor overwrite the user's layout.toml. It boots
        // a fresh "main" workspace and persists to a temp file instead,
        // unless an explicit --layout override was given.
        let layout_path = match (server_mode, layout_override) {
            (true, Some(p)) => p,
            (true, None) => std::env::temp_dir().join("termrs-server-layout.toml"),
            (false, opt) => opt.unwrap_or_else(|| config.layout_path()),
        };
        let shell = config.general.shell.clone();
        let scrollback = config.general.scrollback;

        let workspaces = if server_mode {
            Workspace::new("main".into(), &shell, scrollback, rt, wake)
                .map(|w| vec![w])
                .unwrap_or_default()
        } else {
            LayoutFile::load(&layout_path)
                .map(|f| f.build(&shell, scrollback, rt, wake))
                .filter(|ws| !ws.is_empty())
                .unwrap_or_else(|| {
                    Workspace::new("main".into(), &shell, scrollback, rt, wake)
                        .map(|w| vec![w])
                        .unwrap_or_default()
                })
        };
        if workspaces.is_empty() {
            return Err("no workspace could be started".into());
        }
        let theme = config.theme.resolve();
        let font_size = config.general.font_size.max(6);
        let db_path = config.commands_db_path();
        let db = match CommandDb::open(&db_path) {
            Ok(d) => Some(d),
            Err(e) => {
                log::warn!("command db: {e}");
                None
            }
        };
        let (mcp_query_tx, mcp_query_rx) = crossbeam_channel::unbounded();
        let (share_query_tx, share_query_rx) = crossbeam_channel::unbounded();
        let mut app = Self {
            config,
            braille: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),            workspaces,
            current: 0,
            prompt: None,
            menu: MenuState::new(Vec::new()),
            menu_open: false,
            palette: None,
            db,
            db_path,
            command_picker: None,
            command_form: None,
            command_args: None,
            cheat_picker: None,
            font_picker: None,
            font_preview: None,
            font_preview_tx: None,
            font_preview_rx: None,
            font_preview_gen: 0,
            font_preview_loading: false,
            font_preview_sent: None,
            font_request: None,
            config_mtime: None,
            reload_tick: 0,
            cwd_seq_seen: 0,
            ai_rx: None,
            cheat_rx: None,
            scrollback: None,
            select: None,
            image: None,
            image_rect: None,
            last_mouse_cell: None,
            about: false,
            status: String::new(),
            should_quit: false,
            throbber: ThrobberState::default(),
            effects: EffectManager::default(),
            pending_fx: Vec::new(),
            closing: Vec::new(),
            last_frame: std::time::Instant::now(),
            layout_path,
            rt: rt.clone(),
            theme,
            wake: wake.clone(),
            font_size,
            zoom_request: None,
            ipc_cmd_rx: ipc.as_ref().map(|s| s.cmd_rx.clone()),
            ipc_resp_tx: ipc.map(|s| s.resp_tx),
            http_cmd_rx: http_server.as_ref().map(|s| s.cmd_rx.clone()),
            http_status_tx: http_server.as_ref().map(|s| s.status_tx.clone()),
            http_screenshot_tx: http_server.map(|s| s.screenshot_tx),
            mcp_servers: std::collections::HashMap::new(),
            mcp_query_tx,
            mcp_query_rx: Some(mcp_query_rx),
            share_sessions: std::collections::HashMap::new(),
            share_query_tx,
            share_query_rx: Some(share_query_rx),
        };
        app.ws_mut().mark_seen();
        // Baseline mtime so hot-reload only fires on later changes.
        app.config_mtime = app
            .config
            .source
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        log::info!(
            "booted with {} workspace(s), layout {}",
            app.workspaces.len(),
            app.layout_path.display()
        );
        Ok(app)
    }

    /// Path the layout is saved to (for the status line).
    pub fn layout_path(&self) -> &std::path::Path {
        &self.layout_path
    }

    /// Advance spinner clock; returns time since last frame for tachyonfx.
    pub fn frame_tick(&mut self) -> tachyonfx::Duration {
        self.throbber.calc_next();
        let now = std::time::Instant::now();
        let dt = now.saturating_duration_since(self.last_frame);
        self.last_frame = now;
        tachyonfx::Duration::from_millis(dt.as_millis().clamp(1, u32::MAX as u128) as u32)
    }

    /// Queue a transition for `pane` (fired by ui once layout is known).
    pub fn queue_fx(&mut self, pane: usize, kind: FxKind) {
        self.pending_fx.push(PendingFx { pane, kind });
    }

    /// Drain queued transitions (ui fires them against computed rects).
    pub fn drain_fx(&mut self) -> Vec<PendingFx> {
        std::mem::take(&mut self.pending_fx)
    }

    /// Persist the layout, finishing any playing close transition first so
    /// the saved tree never contains a pane the user already closed. Returns
    /// the filesystem error so the caller can show it.
    pub fn save_layout(&mut self) -> Result<(), String> {
        self.complete_closing();
        let file = LayoutFile::from_workspaces(&self.workspaces);
        match file.save(&self.layout_path) {
            Ok(()) => {
                log::info!(
                    "layout saved to {} ({} workspace(s))",
                    self.layout_path.display(),
                    file.workspaces.len()
                );
                Ok(())
            }
            Err(e) => {
                log::warn!("layout save failed: {e}");
                Err(e)
            }
        }
    }

    /// Finish every playing close transition immediately (forced removal).
    pub fn complete_closing(&mut self) {
        while let Some(c) = self.closing.pop() {
            let empty = match self.workspaces.get_mut(c.ws) {
                Some(ws) => ws.remove_leaf(c.id),
                None => false,
            };
            if empty {
                self.drop_workspace(c.ws);
            }
        }
    }

    /// Complete close transitions whose effect has played out.
    /// Called every frame from [`App::poll_panes`].
    fn poll_closing(&mut self) {
        let now = std::time::Instant::now();
        let mut i = 0;
        while i < self.closing.len() {
            if now >= self.closing[i].until {
                let c = self.closing.remove(i);
                let empty = match self.workspaces.get_mut(c.ws) {
                    Some(ws) => ws.remove_leaf(c.id),
                    None => false,
                };
                if empty {
                    self.drop_workspace(c.ws);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Drop workspace `idx`, shifting pending-close indices above the gap.
    /// Callers complete relevant transitions first, so no live entry ever
    /// references the removed index. Quits when nothing remains.
    fn drop_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() {
            return;
        }
        self.workspaces.remove(idx);
        for c in &mut self.closing {
            if c.ws > idx {
                c.ws -= 1;
            }
        }
        if self.workspaces.is_empty() {
            self.should_quit = true;
            return;
        }
        self.current = self.current.min(self.workspaces.len() - 1);
        self.ws_mut().mark_seen();
    }

    /// Milliseconds the close transition should play, or 0 for instant
    /// removal (fx disabled, unknown effect, or zero duration).
    fn close_delay_ms(&self) -> u32 {
        self.config
            .fx
            .resolve(FxKind::Close)
            .map(|(_, ms)| ms)
            .unwrap_or(0)
    }

    /// Route one exited shell: last pane of the last workspace quits,
    /// last pane of a workspace drops it, otherwise the pane plays its
    /// close transition (or vanishes instantly with fx off).
    fn on_pane_died(&mut self, ws_idx: usize, id: usize) {
        log::debug!("pane {id} in workspace {ws_idx} exited");
        let Some(ws) = self.workspaces.get(ws_idx) else {
            return;
        };
        if self.workspaces.len() == 1 && ws.leaf_ids().len() <= 1 {
            // `exit` in the only pane: the window goes with the shell.
            log::info!("last shell exited; quitting");
            self.should_quit = true;
            return;
        }
        if ws.leaf_ids().len() <= 1 {
            self.drop_workspace(ws_idx);
            return;
        }
        // Focus a survivor when the dead pane had focus.
        if self.workspaces[ws_idx].focused == id {
            let survivor = self.workspaces[ws_idx]
                .leaf_ids()
                .into_iter()
                .find(|&leaf| leaf != id);
            if let Some(s) = survivor {
                self.workspaces[ws_idx].focused = s;
            }
        }
        match self.close_delay_ms() {
            0 => {
                self.workspaces[ws_idx].remove_leaf(id);
            }
            ms => {
                self.closing.push(ClosingPane {
                    ws: ws_idx,
                    id,
                    until: std::time::Instant::now()
                        + std::time::Duration::from_millis(ms as u64),
                });
                self.queue_fx(id, FxKind::Close);
            }
        }
    }

    /// Current workspace.
    pub fn ws(&self) -> &Workspace {
        &self.workspaces[self.current]
    }

    /// Current workspace (mutable).
    pub fn ws_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.current]
    }

    /// Active prompt, if any.
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    /// Active prompt (mutable, for event routing + rendering).
    pub fn prompt_mut(&mut self) -> Option<&mut Prompt> {
        self.prompt.as_mut()
    }

    /// Inbox menu state (for rendering the always-visible menubar).
    pub fn menu_mut(&mut self) -> &mut MenuState<WsAction> {
        &mut self.menu
    }

    /// Resolved theme palette (for renderers).
    pub fn theme_colors(&self) -> ThemeColors {
        self.theme
    }

    /// Cursor cell of the focused pane as `(inner area, row, col)`, or `None`
    /// when there is no visible cursor. The window maps this to pixels for the
    /// GPU-drawn bar/underline cursor.
    pub fn cursor_cell(&self) -> Option<(Rect, u16, u16)> {
        // While selecting, the selection overlay owns the cursor: hide the
        // GPU bar/underline so only the highlighted end shows.
        if self.select.is_some() {
            return None;
        }
        let ws = self.ws();
        let pane = ws.pane(ws.focused)?;
        if pane.dead || pane.screen().hide_cursor() {
            return None;
        }
        let (row, col) = pane.screen().cursor_position();
        let inner = Block::default()
            .borders(Borders::ALL)
            .inner(ws.pane_rect(ws.focused)?);
        if col >= inner.width || row >= inner.height {
            return None;
        }
        Some((inner, row, col))
    }

    /// Scrollback viewer (for overlay rendering).
    pub fn scrollback_mut(&mut self) -> Option<&mut ScrollbackView> {
        self.scrollback.as_mut()
    }

    /// Active visual selection (for the highlight overlay).
    pub fn select(&self) -> Option<&SelectMode> {
        self.select.as_ref()
    }

    /// Open image (for overlay rendering).
    pub fn image_ref(&self) -> Option<&ImageView> {
        self.image.as_ref()
    }

    /// Aspect-fitted popup cell rect for the image overlay (window host).
    pub fn image_rect(&self) -> Option<Rect> {
        self.image_rect
    }

    /// Set the fitted popup rect (window host, before each draw).
    pub fn set_image_rect(&mut self, rect: Option<Rect>) {
        self.image_rect = rect;
    }

    /// Set the open image (self-test driver).
    pub fn set_image_for_test(&mut self, img: ImageView) {
        self.image = Some(img);
    }

    /// Take the open image's pixels (moved, not cloned) for upload.
    /// Leaves the image open (path/dims) but with empty pixels.
    pub fn take_image_rgba(&mut self) -> Option<Vec<u8>> {
        self.image.as_mut().map(|i| std::mem::take(&mut i.rgba))
    }

    /// Poll every workspace (background panes keep running).
    pub fn poll_panes(&mut self) {
        // Collect deaths first; route them in descending workspace order so
        // dropping a workspace never shifts an unprocessed index.
        let mut deaths = Vec::new();
        for (i, w) in self.workspaces.iter_mut().enumerate() {
            for id in w.poll_panes() {
                deaths.push((i, id));
            }
        }
        deaths.sort_by(|a, b| b.cmp(a));
        for (i, id) in deaths {
            self.on_pane_died(i, id);
        }
        self.poll_closing();
        self.poll_ai();
        self.poll_cheat();
        self.poll_font_preview();
        self.poll_cwd_reply();
        self.poll_ipc();
        self.poll_http();
        self.poll_mcp();
        self.poll_share();
        self.hot_reload_check();
    }

    /// Handle pending HTTP server commands.
    fn poll_http(&mut self) {
        let Some(rx) = &self.http_cmd_rx else { return };
        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        for cmd in commands {
            match cmd {
                crate::server::ServerCommand::KeyPress { key, modifier } => {
                    self.handle_http_key(key, modifier);
                }
                crate::server::ServerCommand::RunCommand(cmd) => {
                    self.handle_http_command(&cmd);
                }
                crate::server::ServerCommand::GetStatus => {
                    let text = self.get_terminal_text();
                    if let Some(tx) = &self.http_status_tx {
                        let _ = tx.send(text);
                    }
                }
                crate::server::ServerCommand::GetScreenshot => {
                    let png = self.get_terminal_screenshot();
                    if let Some(tx) = &self.http_screenshot_tx {
                        let _ = tx.send(png);
                    }
                }
                crate::server::ServerCommand::Quit => {
                    self.should_quit = true;
                }
            }
        }
    }

    /// Handle a key press from the HTTP server.
    fn handle_http_key(&mut self, key: String, modifier: Option<String>) {
        use crate::keys::{Key, KeyKind, KeyPress, Mods};
        let mut mods = Mods::empty();
        if let Some(m) = modifier {
            if m.contains("Ctrl") { mods.insert(Mods::CONTROL); }
            if m.contains("Alt") { mods.insert(Mods::ALT); }
            if m.contains("Shift") { mods.insert(Mods::SHIFT); }
        }
        let k = match key.as_str() {
            "Enter" => Key::Enter,
            "Escape" => Key::Esc,
            "Backspace" => Key::Backspace,
            "Tab" => Key::Tab,
            "Up" => Key::Up,
            "Down" => Key::Down,
            "Left" => Key::Left,
            "Right" => Key::Right,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "Delete" => Key::Delete,
            "Insert" => Key::Insert,
            "F1" => Key::F(1),
            "F2" => Key::F(2),
            "F3" => Key::F(3),
            "F4" => Key::F(4),
            "F5" => Key::F(5),
            "F6" => Key::F(6),
            "F7" => Key::F(7),
            "F8" => Key::F(8),
            "F9" => Key::F(9),
            "F10" => Key::F(10),
            "F11" => Key::F(11),
            "F12" => Key::F(12),
            k if k.len() == 1 => Key::Char(k.chars().next().unwrap()),
            _ => return,
        };
        let press = KeyPress {
            key: k,
            mods,
            text: None,
            kind: KeyKind::Press,
        };
        self.handle_key(press);
        // Real keyboards always produce down/up pairs. Follow the
        // synthetic press with its release: in win32-input-mode a key
        // left logically down corrupts the next identical key-down
        // (see handle_http_command), and releases are dropped on the
        // legacy VT path, so this is a no-op everywhere else.
        let release = KeyPress {
            key: k,
            mods,
            text: None,
            kind: KeyKind::Release,
        };
        self.handle_key(release);
    }

    /// Handle a command string from the HTTP server: inject it as literal
    /// input bytes plus Enter, in a single write.
    ///
    /// A previous implementation synthesized one key-press event per
    /// character. In win32-input-mode panes those become a microsecond
    /// burst of key-down records with no key-up in between, and ConPTY
    /// coalesces back-to-back identical key-downs, silently dropping
    /// doubled characters (`a--b` arrived as `a-b`, `C++` as `C+`).
    /// A single raw write takes the same path as pastes and macros and
    /// preserves every byte.
    fn handle_http_command(&mut self, cmd: &str) {
        let ws = self.ws_mut();
        let Some(p) = ws.pane_mut(ws.focused) else {
            return;
        };
        let mut buf = Vec::with_capacity(cmd.len() + 1);
        buf.extend_from_slice(cmd.as_bytes());
        buf.push(b'\r');
        p.write(&buf);
    }

    /// Get the terminal text content of the focused pane.
    fn get_terminal_text(&self) -> String {
        let ws = self.ws();
        let pane = match ws.pane(ws.focused) {
            Some(p) => p,
            None => return String::new(),
        };
        let screen = pane.screen();
        let (rows, cols) = screen.size();
        let mut lines = Vec::new();
        for r in 0..rows {
            let mut line = String::new();
            for c in 0..cols {
                let cell = screen.cell(r, c);
                let text = cell.map(|c| c.contents()).unwrap_or("");
                if text.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(text);
                }
            }
            lines.push(line.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// Get a PNG screenshot of the terminal.
    fn get_terminal_screenshot(&self) -> Vec<u8> {
        self.screenshot_pane(self.ws().focused)
    }

    /// PNG of any pane's current screen (used by the HTTP server and MCP).
    fn screenshot_pane(&self, pane_id: usize) -> Vec<u8> {
        let pane = match self.find_pane(pane_id) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let screen = pane.screen();
        let (rows, cols) = screen.size();
        let width = cols as u32;
        let height = rows as u32;
        let mut img = image::RgbaImage::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let cell = screen.cell(y as u16, x as u16);
                let fg = cell.map(|c| c.fgcolor()).unwrap_or(vt100::Color::Default);
                let bg = cell.map(|c| c.bgcolor()).unwrap_or(vt100::Color::Default);
                let color = match fg {
                    vt100::Color::Default => [0, 0, 0, 255],
                    vt100::Color::Idx(i) => {
                        let colors = [
                            [0, 0, 0, 255], [128, 0, 0, 255], [0, 128, 0, 255], [128, 128, 0, 255],
                            [0, 0, 128, 255], [128, 0, 128, 255], [0, 128, 128, 255], [192, 192, 192, 255],
                            [128, 128, 128, 255], [255, 0, 0, 255], [0, 255, 0, 255], [255, 255, 0, 255],
                            [0, 0, 255, 255], [255, 0, 255, 255], [0, 255, 255, 255], [255, 255, 255, 255],
                        ];
                        colors.get(i as usize).copied().unwrap_or([0, 0, 0, 255])
                    }
                    vt100::Color::Rgb(r, g, b) => [r, g, b, 255],
                };
                let _ = bg;
                img.put_pixel(x, y, image::Rgba(color));
            }
        }
        let mut buf = Vec::new();
        let _ = img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png);
        buf
    }

    /// Find a pane by id in any workspace (MCP servers target a fixed pane,
    /// which need not be the focused one).
    fn find_pane(&self, id: usize) -> Option<&crate::pty::Pane> {
        self.workspaces.iter().find_map(|w| w.pane(id))
    }

    fn find_pane_mut(&mut self, id: usize) -> Option<&mut crate::pty::Pane> {
        self.workspaces
            .iter_mut()
            .find(|w| w.leaf_ids().contains(&id))
            .and_then(|w| w.pane_mut(id))
    }

    /// Attach a pane-scoped MCP server to a pane.
    ///
    /// `split` chooses where the pane comes from: `Some(dir)` splits the
    /// focused pane in that direction and uses the new one; `None` reuses the
    /// focused pane as-is. Re-running on a pane that already hosts a server
    /// replaces it (the old server is stopped).
    fn open_mcp_pane(&mut self, split: Option<SplitDir>) {
        let pane_id = match split {
            Some(dir) => {
                let shell = self.config.general.shell.clone();
                let sb = self.config.general.scrollback;
                let rt = self.rt.clone();
                match self.ws_mut().split(dir, &shell, sb, &rt) {
                    Ok(id) => {
                        self.queue_fx(id, FxKind::Fresh);
                        id
                    }
                    Err(e) => {
                        self.status = format!("MCP pane failed: {e}");
                        return;
                    }
                }
            }
            None => self.ws().focused,
        };
        self.attach_mcp(pane_id);
    }

    /// Start (or restart) the MCP server bound to `pane_id`.
    fn attach_mcp(&mut self, pane_id: usize) {
        if let Some(mut old) = self.mcp_servers.remove(&pane_id) {
            old.stop();
        }
        let rt = self.rt.clone();
        let port = self.config.general.mcp_port;
        match crate::mcp::start_pane_mcp(&rt, self.mcp_query_tx.clone(), pane_id, port) {
            Ok(server) => {
                let port = server.port;
                if let Some(p) = self.find_pane_mut(pane_id) {
                    p.mcp_status = Some(server.shared.status());
                }
                self.mcp_servers.insert(pane_id, server);
                self.status = format!("MCP pane {pane_id}: http://127.0.0.1:{port}/mcp");
                log::info!("MCP server for pane {pane_id} listening on 127.0.0.1:{port}");
            }
            Err(e) => {
                self.status = format!("MCP server failed: {e}");
            }
        }
    }

    /// Answer pending MCP tool calls, then stop servers whose pane is gone.
    fn poll_mcp(&mut self) {
        let queries: Vec<crate::mcp::McpQuery> = match &self.mcp_query_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for q in queries {
            let pane_id = q.pane_id;
            let reply = self.mcp_handle_query(pane_id, q.kind);
            let _ = q.reply.send(reply);
        }
        self.mcp_reconcile();
    }

    fn mcp_handle_query(
        &mut self,
        pane_id: usize,
        kind: crate::mcp::McpQueryKind,
    ) -> crate::mcp::McpReply {
        use crate::mcp::{McpQueryKind as K, McpReply as R};
        match kind {
            K::Run(command) => {
                let Some(p) = self.find_pane_mut(pane_id) else {
                    return R::Err(format!("pane {pane_id} is gone"));
                };
                let mut bytes = command.into_bytes();
                bytes.push(b'\r');
                p.write(&bytes);
                R::Ok
            }
            K::Key { key, modifier } => {
                self.mcp_press_key(pane_id, &key, modifier.as_deref());
                R::Ok
            }
            K::Mouse {
                col,
                row,
                button,
                modifier,
            } => self.mcp_mouse_click(pane_id, col, row, button, modifier.as_deref()),
            K::Drag {
                from,
                to,
                button,
                modifier,
            } => self.mcp_mouse_drag(pane_id, from, to, button, modifier.as_deref()),
            K::Interrupt => match self.find_pane_mut(pane_id) {
                Some(p) => {
                    p.interrupt();
                    R::Ok
                }
                None => R::Err(format!("pane {pane_id} is gone")),
            },
            K::Screen => match self.find_pane(pane_id) {
                Some(p) => R::Text(p.grid_lines().join("\n")),
                None => R::Err(format!("pane {pane_id} is gone")),
            },
            K::Screenshot => {
                let png = self.screenshot_pane(pane_id);
                if png.is_empty() {
                    R::Err(format!("pane {pane_id} has no image"))
                } else {
                    R::Png(png)
                }
            }
            K::Info => match self.mcp_pane_info(pane_id) {
                Ok(json) => R::Text(json),
                Err(e) => R::Err(e),
            },
        }
    }

    /// Parse an MCP modifier string ("Ctrl"/"Alt"/"Shift") into key modifiers.
    fn mcp_mods(modifier: Option<&str>) -> Mods {
        let mut mods = Mods::empty();
        if let Some(m) = modifier {
            if m.contains("Ctrl") {
                mods.insert(Mods::CONTROL);
            }
            if m.contains("Alt") {
                mods.insert(Mods::ALT);
            }
            if m.contains("Shift") {
                mods.insert(Mods::SHIFT);
            }
        }
        mods
    }

    /// Make `pane_id`'s workspace active and return its cached draw rect plus
    /// the inner (border-inset) grid size. Mouse helpers translate pane-relative
    /// cells through this rect; `mouse_button`/`mouse_move` read `self.ws()`.
    fn mcp_pane_grid(&mut self, pane_id: usize) -> Result<(Rect, u16, u16), String> {
        let Some(ws_idx) = self.workspaces.iter().position(|w| w.pane(pane_id).is_some()) else {
            return Err(format!("pane {pane_id} is gone"));
        };
        if ws_idx != self.current {
            self.switch_to(ws_idx);
        }
        let Some(rect) = self.ws().pane_rect(pane_id) else {
            return Err(format!("pane {pane_id} is not laid out yet"));
        };
        // Panes draw a one-cell border, so the inner grid starts at +1.
        Ok((rect, rect.width.saturating_sub(2), rect.height.saturating_sub(2)))
    }

    /// True when a mouse report at either press or release reached the pane.
    fn mouse_forwarded(o: MouseClickOutcome) -> bool {
        matches!(
            o,
            MouseClickOutcome::Forwarded | MouseClickOutcome::OpenedUrl
        )
    }

    /// Emulate a mouse click at zero-based cell `(col, row)` inside the pane's
    /// text grid. [`App::mouse_button`] wants full-window coordinates, so
    /// translate through the pane's cached draw rect (the border insets the
    /// grid by one cell), then press and release the button. Reports whether
    /// the pane actually forwarded the click (its app must track the mouse).
    fn mcp_mouse_click(
        &mut self,
        pane_id: usize,
        col: u16,
        row: u16,
        button: crate::mouse::MouseButton,
        modifier: Option<&str>,
    ) -> crate::mcp::McpReply {
        use crate::mcp::McpReply as R;
        let (rect, inner_w, inner_h) = match self.mcp_pane_grid(pane_id) {
            Ok(v) => v,
            Err(e) => return R::Err(e),
        };
        if col >= inner_w || row >= inner_h {
            return R::Err(format!(
                "({col}, {row}) is outside the pane grid ({inner_w}x{inner_h})"
            ));
        }
        let (full_col, full_row) = (rect.x + 1 + col, rect.y + 1 + row);
        let mods = Self::mcp_mods(modifier);
        let pressed = self.mouse_button(full_col, full_row, button, true, mods);
        let released = self.mouse_button(full_col, full_row, button, false, mods);
        if button == crate::mouse::MouseButton::Left {
            self.focus_pane_at(full_col, full_row);
        }
        if Self::mouse_forwarded(pressed) || Self::mouse_forwarded(released) {
            R::Text(format!("clicked {button:?} at ({col}, {row})"))
        } else {
            R::Err(format!(
                "pane {pane_id} did not accept the click (its app must enable mouse tracking)"
            ))
        }
    }

    /// Emulate a drag: press `button` at `from`, drag through intermediate
    /// cells to `to`, then release. Motion reports need the pane's app to have
    /// enabled drag/any-motion tracking (DECSET 1002/1003).
    fn mcp_mouse_drag(
        &mut self,
        pane_id: usize,
        from: (u16, u16),
        to: (u16, u16),
        button: crate::mouse::MouseButton,
        modifier: Option<&str>,
    ) -> crate::mcp::McpReply {
        use crate::mcp::McpReply as R;
        let (rect, inner_w, inner_h) = match self.mcp_pane_grid(pane_id) {
            Ok(v) => v,
            Err(e) => return R::Err(e),
        };
        if from.0 >= inner_w || from.1 >= inner_h || to.0 >= inner_w || to.1 >= inner_h {
            return R::Err(format!(
                "drag {from:?}..{to:?} is outside the pane grid ({inner_w}x{inner_h})"
            ));
        }
        let mods = Self::mcp_mods(modifier);
        let (full_col, full_row) = (rect.x + 1 + from.0, rect.y + 1 + from.1);
        let (end_col, end_row) = (rect.x + 1 + to.0, rect.y + 1 + to.1);
        let pressed = self.mouse_button(full_col, full_row, button, true, mods);
        // Walk the straight line between the two cells, one report per step,
        // so apps that follow the pointer see the whole gesture (a same-cell
        // drag is just press + release).
        let (x0, y0) = (from.0 as i32, from.1 as i32);
        let (x1, y1) = (to.0 as i32, to.1 as i32);
        let steps = (x1 - x0).abs().max((y1 - y0).abs());
        let mut moved = false;
        for i in 1..=steps {
            let x = (x0 + (x1 - x0) * i / steps) as u16;
            let y = (y0 + (y1 - y0) * i / steps) as u16;
            moved |= self.mcp_write_motion(pane_id, x, y, button, mods);
        }
        let released = self.mouse_button(end_col, end_row, button, false, mods);
        if button == crate::mouse::MouseButton::Left {
            self.focus_pane_at(full_col, full_row);
        }
        if Self::mouse_forwarded(pressed) || moved || Self::mouse_forwarded(released) {
            R::Text(format!("dragged {button:?} from {from:?} to {to:?}"))
        } else {
            R::Err(format!(
                "pane {pane_id} did not accept the drag (its app must enable mouse tracking)"
            ))
        }
    }

    /// Forward a held-button motion report at pane-relative `(col, row)`.
    /// Returns whether the pane's app asked for motion and the bytes went out.
    fn mcp_write_motion(
        &mut self,
        pane_id: usize,
        col: u16,
        row: u16,
        button: crate::mouse::MouseButton,
        mods: Mods,
    ) -> bool {
        let Some(rect) = self.ws().pane_rect(pane_id) else {
            return false;
        };
        let (full_col, full_row) = (rect.x + 1 + col, rect.y + 1 + row);
        let Some((id, c, r)) = self.inner_at(full_col, full_row) else {
            return false;
        };
        let state = match self.ws().pane(id) {
            Some(pane) => pane.mouse_state(),
            None => return false,
        };
        if !state.wants_motion(true) {
            return false;
        }
        let bytes = crate::mouse::encode_motion(Some(button), c, r, mods, state.encoding);
        if let Some(pane) = self.ws_mut().pane_mut(id) {
            pane.write(&bytes);
            true
        } else {
            false
        }
    }

    /// Press a named key in a specific pane (mirrors the HTTP `/key` path).
    fn mcp_press_key(&mut self, pane_id: usize, key: &str, modifier: Option<&str>) {
        use crate::keys::{Key, KeyKind, KeyPress};
        let mods = Self::mcp_mods(modifier);
        let k = match key {
            "Enter" => Key::Enter,
            "Escape" => Key::Esc,
            "Backspace" => Key::Backspace,
            "Tab" => Key::Tab,
            "Up" => Key::Up,
            "Down" => Key::Down,
            "Left" => Key::Left,
            "Right" => Key::Right,
            "Home" => Key::Home,
            "End" => Key::End,
            "PageUp" => Key::PageUp,
            "PageDown" => Key::PageDown,
            "Delete" => Key::Delete,
            "Insert" => Key::Insert,
            "F1" => Key::F(1),
            "F2" => Key::F(2),
            "F3" => Key::F(3),
            "F4" => Key::F(4),
            "F5" => Key::F(5),
            "F6" => Key::F(6),
            "F7" => Key::F(7),
            "F8" => Key::F(8),
            "F9" => Key::F(9),
            "F10" => Key::F(10),
            "F11" => Key::F(11),
            "F12" => Key::F(12),
            other if other.chars().count() == 1 => {
                Key::Char(other.chars().next().expect("one char"))
            }
            _ => return,
        };
        let Some(p) = self.find_pane_mut(pane_id) else {
            return;
        };
        for kind in [KeyKind::Press, KeyKind::Release] {
            p.write_key(&KeyPress {
                key: k,
                mods,
                text: None,
                kind,
            });
        }
    }

    /// JSON description of a pane for the MCP `termrs_pane_info` tool.
    fn mcp_pane_info(&self, pane_id: usize) -> Result<String, String> {
        let pane = self.find_pane(pane_id).ok_or_else(|| format!("pane {pane_id} is gone"))?;
        let workspace = self
            .workspaces
            .iter()
            .position(|w| w.pane(pane_id).is_some())
            .unwrap_or(0);
        Ok(serde_json::json!({
            "pane_id": pane.id,
            "workspace": workspace,
            "title": pane.title,
            "cwd": pane.cwd().to_string_lossy(),
            "dead": pane.dead,
        })
        .to_string())
    }

    /// Stop MCP servers whose pane was closed; refresh the rest's status line.
    fn mcp_reconcile(&mut self) {
        let ids: Vec<usize> = self.mcp_servers.keys().copied().collect();
        for id in ids {
            if self.find_pane(id).is_none()
                && let Some(mut server) = self.mcp_servers.remove(&id)
            {
                server.stop();
                log::info!("MCP server for pane {id} stopped (pane closed)");
            }
        }
        let statuses: Vec<(usize, String)> = self
            .mcp_servers
            .iter()
            .map(|(id, server)| (*id, server.shared.status()))
            .collect();
        for (id, text) in statuses {
            if let Some(p) = self.find_pane_mut(id) {
                p.mcp_status = Some(text);
            }
        }
    }

    /// Start (or toggle off) sharing the focused pane over iroh.
    fn share_focused(&mut self) {
        let pane_id = self.ws().focused;
        if self.share_sessions.contains_key(&pane_id) {
            self.stop_sharing(pane_id);
            return;
        }
        let code = crate::share::generate_code(self.config.share.code_len);
        let page_url = self.config.share.page_url.clone();
        let allow_control = self.config.share.allow_control;
        match crate::share::start(
            &self.rt,
            self.share_query_tx.clone(),
            pane_id,
            code,
            &page_url,
            allow_control,
        ) {
            Ok(session) => {
                let out_tx = session.shared.out_tx.clone();
                let link = session.link.clone();
                let code = session.code.clone();
                if let Some(p) = self.find_pane_mut(pane_id) {
                    p.set_share_out(Some(out_tx));
                }
                log::info!(
                    "share pane {pane_id}: code {code}, ticket {} chars",
                    session.ticket.len()
                );
                self.share_sessions.insert(pane_id, session);
                self.status = match crate::clipboard::copy(&link) {
                    Ok(()) => format!("sharing pane {pane_id} · code {code} · link copied"),
                    Err(e) => format!("sharing pane {pane_id} · code {code} · copy failed: {e}"),
                };
            }
            Err(e) => self.status = format!("share failed: {e}"),
        }
    }

    /// Stop sharing `pane_id` (no-op with a status note when it is not shared).
    fn stop_sharing(&mut self, pane_id: usize) {
        match self.share_sessions.remove(&pane_id) {
            Some(mut session) => {
                session.stop();
                if let Some(p) = self.find_pane_mut(pane_id) {
                    p.set_share_out(None);
                    p.share_status = None;
                }
                log::info!("share pane {pane_id} stopped");
                self.status = format!("stopped sharing pane {pane_id}");
            }
            None => self.status = "pane is not being shared".into(),
        }
    }

    /// Answer pending share-viewer queries, then stop sessions whose pane is
    /// gone and refresh the rest's status line.
    fn poll_share(&mut self) {
        let queries: Vec<crate::share::ShareQuery> = match &self.share_query_rx {
            Some(rx) => rx.try_iter().collect(),
            None => Vec::new(),
        };
        for q in queries {
            let reply = self.share_handle_query(q.pane_id, q.kind);
            let _ = q.reply.send(reply);
        }
        self.share_reconcile();
    }

    fn share_handle_query(
        &mut self,
        pane_id: usize,
        kind: crate::share::ShareQueryKind,
    ) -> crate::share::ShareReply {
        use crate::share::{ShareQueryKind as K, ShareReply as R};
        match kind {
            K::Snapshot { cols, rows } => match self.share_snapshot(pane_id, cols, rows) {
                Ok(bytes) => R::Bytes(bytes),
                Err(e) => R::Err(e),
            },
            K::Input(bytes) => match self.find_pane_mut(pane_id) {
                Some(p) => {
                    p.write(&bytes);
                    R::Ok
                }
                None => R::Err(format!("pane {pane_id} is gone")),
            },
        }
    }

    /// Build a viewer snapshot: clear, re-assert input modes (application
    /// cursor/keypad, bracketed paste, mouse), repaint the screen, place the
    /// cursor, and prefix the pane's grid size so the viewer can match it.
    fn share_snapshot(
        &self,
        pane_id: usize,
        _cols: u16,
        _rows: u16,
    ) -> Result<Vec<u8>, String> {
        let pane = self
            .find_pane(pane_id)
            .ok_or_else(|| format!("pane {pane_id} is gone"))?;
        let screen = pane.screen();
        let (rows, cols) = screen.size();
        let mut ansi = Vec::new();
        ansi.extend_from_slice(b"\x1b[2J\x1b[H");
        ansi.extend_from_slice(&screen.input_mode_formatted());
        ansi.extend_from_slice(&screen.contents_formatted());
        let (r, c) = screen.cursor_position();
        ansi.extend_from_slice(format!("\x1b[{};{}H", r + 1, c + 1).as_bytes());
        Ok(termrs_share_proto::encode_snapshot(cols, rows, &ansi))
    }

    /// Stop share sessions whose pane closed; refresh the rest's status line.
    fn share_reconcile(&mut self) {
        let ids: Vec<usize> = self.share_sessions.keys().copied().collect();
        for id in ids {
            if self.find_pane(id).is_none()
                && let Some(mut session) = self.share_sessions.remove(&id)
            {
                session.stop();
                log::info!("share pane {id} stopped (pane closed)");
            }
        }
        let statuses: Vec<(usize, String)> = self
            .share_sessions
            .iter()
            .map(|(id, s)| (*id, format!("SHARE · code {}", s.code)))
            .collect();
        for (id, text) in statuses {
            if let Some(p) = self.find_pane_mut(id) {
                p.share_status = Some(text);
            }
        }
    }

    /// Handle pending IPC commands from external CLI clients.
    fn poll_ipc(&mut self) {
        let Some(rx) = &self.ipc_cmd_rx else { return };
        let mut commands = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            commands.push(cmd);
        }
        for cmd in commands {
            match cmd {
                crate::ipc::IpcCommand::SplitPane { args, dir } => {
                    let resp = self.handle_split_pane_ipc(args, dir);
                    if let Some(tx) = &self.ipc_resp_tx {
                        let _ = tx.send(resp);
                    }
                }
                crate::ipc::IpcCommand::ListPanes => {
                    let panes: Vec<crate::ipc::PaneInfo> = self.workspaces.iter().enumerate().flat_map(|(ws_idx, ws)| {
                        ws.leaf_ids().into_iter().filter_map(move |id| {
                            let pane = ws.pane(id)?;
                            Some(crate::ipc::PaneInfo {
                                pane_id: id,
                                workspace: ws_idx,
                                title: pane.title.clone(),
                                dead: pane.dead,
                                cwd: pane.cwd().to_string_lossy().into_owned(),
                            })
                        })
                    }).collect();
                    if let Some(tx) = &self.ipc_resp_tx {
                        let _ = tx.send(crate::ipc::Response {
                            ok: true,
                            pane_id: None,
                            panes: Some(panes),
                            error: None,
                        });
                    }
                }
            }
        }
    }

    /// Handle a split-pane IPC command: split the focused pane and type the
    /// command into the new pane.
    fn handle_split_pane_ipc(&mut self, args: Vec<String>, _dir: Option<PathBuf>) -> crate::ipc::Response {
        let shell = self.config.general.shell.clone();
        let scrollback = self.config.general.scrollback;
        let rt = self.rt.clone();
        match self.ws_mut().split(crate::workspace::SplitDir::Vertical, &shell, scrollback, &rt) {
            Ok(new_id) => {
                let cmd = args.join(" ");
                if !cmd.is_empty()
                    && let Some(p) = self.ws_mut().pane_mut(new_id) {
                        p.write(format!("{cmd}\r").as_bytes());
                    }
                crate::ipc::Response {
                    ok: true,
                    pane_id: Some(new_id),
                    panes: None,
                    error: None,
                }
            }
            Err(e) => crate::ipc::Response {
                ok: false,
                pane_id: None,
                panes: None,
                error: Some(e),
            },
        }
    }

    /// When the shell answers a cwd query, rebuild the prompt's completion so
    /// it reflects the fresh directory immediately (no re-pressing ctrl+i).
    fn poll_cwd_reply(&mut self) {
        if self.prompt.is_none() {
            return;
        }
        let fid = self.ws().focused;
        let seq = match self.ws().pane(fid) {
            Some(p) => p.cwd_seq(),
            None => return,
        };
        if seq != self.cwd_seq_seen {
            self.cwd_seq_seen = seq;
            self.update_prompt_ghost();
        }
    }

    /// Dispatch one key event. True = consumed as app binding.
    ///
    /// Overlays are dispatched by [`Mode`] (State pattern); ordinary keys go
    /// through a table-driven [`App::keymap`] lookup (Command pattern).
    pub fn handle_key(&mut self, ev: KeyPress) -> bool {
        // Releases never trigger bindings/overlays: they only exist so a
        // win32-input-mode child can observe a key being let go.
        if ev.kind == KeyKind::Release {
            if self.mode() == Mode::Normal {
                let ws = self.ws_mut();
                if let Some(p) = ws.pane_mut(ws.focused) {
                    p.write_key(&ev);
                }
            }
            return false;
        }
        match self.mode() {
            Mode::Image => {
                if matches!(ev.key, Key::Esc | Key::Char('q')) {
                    self.image = None;
                }
                return true;
            }
            Mode::Scrollback => {
                self.handle_scrollback_key(ev);
                return true;
            }
            Mode::Select => {
                self.handle_select_key(ev);
                return true;
            }
            Mode::Palette => {
                self.handle_palette_key(ev);
                return true;
            }
            Mode::CommandForm => {
                self.handle_command_form_key(ev);
                return true;
            }
            Mode::CommandArgs => {
                self.handle_command_args_key(ev);
                return true;
            }
            Mode::Commands => {
                self.handle_command_picker_key(ev);
                return true;
            }
            Mode::CheatPicker => {
                self.handle_cheat_picker_key(ev);
                return true;
            }
            Mode::FontPicker => {
                self.handle_font_picker_key(ev);
                return true;
            }
            Mode::Menu => {
                self.handle_menu_key(ev);
                return true;
            }
            Mode::Prompt => {
                self.handle_prompt_key(&ev);
                return true;
            }
            Mode::Normal => {}
        }

        // Help overlay is non-modal: its key toggles, Esc dismisses.
        if keys::matches_any(&ev, &self.config.keys.help) {
            self.about = !self.about;
            return true;
        }
        if self.about && matches!(ev.key, Key::Esc) {
            self.about = false;
            return true;
        }
        // Table-driven bindings (order = priority).
        if let Some(cmd) = self.lookup_command(&ev) {
            self.run_command(cmd);
            return true;
        }
        // Typed-text macros ([[macros]]): send text to the focused pane.
        if let Some((send, enter)) = self
            .config
            .macros
            .iter()
            .find(|m| keys::matches_any(&ev, &m.keys))
            .map(|m| (m.send.clone(), m.enter))
        {
            log::debug!("macro sends {} bytes (enter={enter})", send.len());
            let ws = self.ws_mut();
            if let Some(p) = ws.pane_mut(ws.focused) {
                p.write(send.as_bytes());
                if enter {
                    p.write(b"\r");
                }
            }
            return true;
        }
        // Ctrl+C interrupts the foreground process (stops a running server).
        // Runs after bindings/macros so a user `ctrl+c` binding still wins;
        // `Pane::write` also upgrades a bare ETX to an interrupt, so pasted
        // or macro-sent 0x03 takes the same path.
        if is_interrupt(&ev) {
            let ws = self.ws_mut();
            if let Some(p) = ws.pane_mut(ws.focused) {
                p.interrupt();
            }
            return false;
        }
        // Regular terminal input -> focused pane of current workspace.
        let ws = self.ws_mut();
        if let Some(p) = ws.pane_mut(ws.focused) {
            p.write_key(&ev);
        }
        false
    }

    /// The active overlay, most-modal first (State pattern).
    pub(crate) fn mode(&self) -> Mode {
        if self.image.is_some() {
            Mode::Image
        } else if self.scrollback.is_some() {
            Mode::Scrollback
        } else if self.select.is_some() {
            Mode::Select
        } else if self.palette.is_some() {
            Mode::Palette
        } else if self.command_form.is_some() {
            Mode::CommandForm
        } else if self.command_args.is_some() {
            Mode::CommandArgs
        } else if self.command_picker.is_some() {
            Mode::Commands
        } else if self.cheat_picker.is_some() {
            Mode::CheatPicker
        } else if self.font_picker.is_some() {
            Mode::FontPicker
        } else if self.menu_open {
            Mode::Menu
        } else if self.prompt.is_some() {
            Mode::Prompt
        } else {
            Mode::Normal
        }
    }

    /// Binding table: config key specs -> command, in priority order.
    /// Single source of truth shared with the palette and tests.
    fn keymap(&self) -> Vec<(Vec<String>, Command)> {
        let k = &self.config.keys;
        vec![
            (k.yank_last.clone(), Command::YankLast),
            (k.select_mode.clone(), Command::SelectMode),
            (k.edit_config.clone(), Command::EditConfig),
            (k.command_palette.clone(), Command::CommandPalette),
            (k.command_save.clone(), Command::SaveCommand),
            (k.commands_picker.clone(), Command::Commands),
            (k.paste.clone(), Command::Paste),
            (k.quit.clone(), Command::Quit),
            (k.split_horizontal.clone(), Command::SplitHorizontal),
            (k.split_vertical.clone(), Command::SplitVertical),
            (k.close_pane.clone(), Command::ClosePane),
            (k.pane_zoom.clone(), Command::ZoomPane),
            (k.focus_next.clone(), Command::FocusNext),
            (k.focus_prev.clone(), Command::FocusPrev),
            (k.focus_left.clone(), Command::FocusLeft),
            (k.focus_right.clone(), Command::FocusRight),
            (k.focus_up.clone(), Command::FocusUp),
            (k.focus_down.clone(), Command::FocusDown),
            (k.resize_left.clone(), Command::ResizeLeft),
            (k.resize_right.clone(), Command::ResizeRight),
            (k.resize_up.clone(), Command::ResizeUp),
            (k.resize_down.clone(), Command::ResizeDown),
            (k.zoom_in.clone(), Command::ZoomIn),
            (k.zoom_out.clone(), Command::ZoomOut),
            (k.zoom_reset.clone(), Command::ZoomReset),
            (k.view_image.clone(), Command::Image),
            (k.workspace_menu.clone(), Command::Inbox),
            (k.workspace_new.clone(), Command::NewWorkspace),
            (k.workspace_next.clone(), Command::WorkspaceNext),
            (k.workspace_prev.clone(), Command::WorkspacePrev),
            (k.scroll_view.clone(), Command::Scrollback),
        ]
    }

    /// First command whose binding matches `ev`, if any.
    fn lookup_command(&self, ev: &KeyPress) -> Option<Command> {
        self.keymap()
            .into_iter()
            .find(|(specs, _)| keys::matches_any(ev, specs))
            .map(|(_, cmd)| cmd)
    }

    /// Prompt-mode keys. Plain Enter submits, Esc cancels, everything else
    /// goes to the textarea (multi-line, Emacs bindings built in).
    /// True when a prompt was active (event consumed).
    fn handle_prompt_key(&mut self, ev: &KeyPress) -> bool {
        if self.prompt.is_none() {
            return false;
        }
        match ev.key {
            Key::Esc => {
                self.prompt = None;
                self.status.clear();
                return true;
            }
            Key::Enter if ev.mods.is_empty() => {
                let Some(p) = self.prompt.take() else { return true };
                self.submit_prompt(p.kind, &p.text());
                return true;
            }
            // Tab accepts the ghost completion.
            Key::Tab => {
                self.accept_ghost();
                return true;
            }
            // Ctrl+V pastes the clipboard into the prompt (path paste).
            Key::Char('v') if ev.mods.contains(Mods::CONTROL) => {
                match crate::clipboard::paste() {
                    Ok(text) => {
                        let text = text.replace(['\r', '\n'], " ");
                        if let Some(p) = &mut self.prompt {
                            p.editor.insert_str(text);
                        }
                    }
                    Err(e) => self.status = format!("paste failed: {e}"),
                }
                self.update_prompt_ghost();
                return true;
            }
            _ => {
                if let Some(p) = &mut self.prompt {
                    p.editor.input(textarea_input(ev));
                }
                self.update_prompt_ghost();
            }
        }
        true
    }

    /// Accept the inline completion (Tab).
    fn accept_ghost(&mut self) {
        let ghost = match &self.prompt {
            Some(p) if !p.ghost.is_empty() => p.ghost.clone(),
            _ => return,
        };
        if let Some(p) = &mut self.prompt {
            p.editor.insert_str(ghost);
        }
        self.update_prompt_ghost();
    }

    /// Compute the ghost completion for the current prompt text.
    fn update_prompt_ghost(&mut self) {
        let Some(kind) = self.prompt.as_ref().map(|p| p.kind) else {
            return;
        };
        if kind != PromptKind::ViewImage {
            if let Some(p) = &mut self.prompt {
                p.ghost.clear();
            }
            return;
        }
        let input = self.prompt.as_ref().map(|p| p.text()).unwrap_or_default();
        let ghost = self.complete_path(&input).unwrap_or_default();
        if let Some(p) = &mut self.prompt {
            p.ghost = ghost;
        }
    }

    /// First path completion for `input`, as the suffix to append.
    /// Matches entries in the focused shell's cwd, preferring images.
    fn complete_path(&self, input: &str) -> Option<String> {
        let last_line = input.lines().last().unwrap_or("");
        let fid = self.ws().focused;
        let cwd = self
            .ws()
            .pane(fid)
            .map(|p| p.cwd().to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        // Split into the directory part and the name being typed. An empty
        // input completes straight from the current directory.
        let trimmed = last_line.trim_end();
        let (dir, base) = match trimmed.rfind(['/', '\\']) {
            Some(i) => (&trimmed[..=i], &trimmed[i + 1..]),
            None => ("", trimmed),
        };
        let dir_path = if dir.is_empty() {
            cwd.clone()
        } else if std::path::Path::new(dir).is_absolute() {
            std::path::PathBuf::from(dir)
        } else {
            cwd.join(dir)
        };
        let entries = std::fs::read_dir(&dir_path).ok()?;

        let base_lc = base.to_lowercase();
        let mut matches: Vec<(u8, String)> = Vec::new();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.len() <= base.len() || !name.to_lowercase().starts_with(&base_lc) {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            // Rank: images first (for the image prompt), then dirs, then files.
            let rank = if crate::image_view::is_image(&name) {
                0
            } else if is_dir {
                1
            } else {
                2
            };
            matches.push((rank, name));
        }
        matches.sort();
        let (_, name) = matches.first()?;
        let mut suffix = name[base.len()..].to_string();
        if dir_path.join(name).is_dir() && !suffix.ends_with(std::path::MAIN_SEPARATOR) {
            suffix.push(std::path::MAIN_SEPARATOR);
        }
        Some(suffix)
    }

    /// Run prompt action: rename the workspace or open an image overlay.
    fn submit_prompt(&mut self, kind: PromptKind, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        match kind {
            PromptKind::RenameWorkspace => {
                let name: String = input.lines().next().unwrap_or("ws").trim().into();
                if !name.is_empty() {
                    self.ws_mut().name = name.clone();
                    self.status = format!("workspace renamed to {name}");
                }
            }
            PromptKind::ViewImage => {
                // Resolve relative paths against the focused shell's cwd
                // (queried from the shell), falling back to the config dir.
                let candidate = std::path::Path::new(input);
                let resolved = if candidate.is_absolute() {
                    candidate.to_path_buf()
                } else {
                    let fid = self.ws().focused;
                    let cwd = self
                        .ws()
                        .pane(fid)
                        .map(|p| p.cwd().to_path_buf())
                        .unwrap_or_else(|| {
                            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                        });
                    let p = cwd.join(candidate);
                    if p.exists() {
                        p
                    } else {
                        self.config.state_dir().join(candidate)
                    }
                };
                let shown = resolved.to_string_lossy().to_string();
                match ImageView::open(&shown) {
                    Ok(v) => {
                        self.status = format!("image: {shown}");
                        self.image = Some(v);
                    }
                    Err(e) => self.status = format!("image err: {e}"),
                }
            }
            PromptKind::AskAi => self.ask_ai(input),
            PromptKind::CheatSheet => self.import_cheat_sheet(input),
        }
    }

    /// Fetch a cheat.sh sheet on a background thread; when it lands, open
    /// the tick-box review picker (nothing is saved until confirmed).
    fn import_cheat_sheet(&mut self, topic: &str) {
        let topic = topic.trim().to_string();
        if topic.is_empty() {
            self.status = "import: empty topic (try e.g. tar)".into();
            return;
        }
        if self.cheat_rx.is_some() {
            self.status = "import: still fetching...".into();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let worker_topic = topic.clone();
        std::thread::spawn(move || {
            let result = crate::cheatsheet::fetch_topic(&worker_topic).map(|text| CheatImport {
                topic: worker_topic.clone(),
                entries: crate::cheatsheet::parse_sheet(&text),
            });
            let _ = tx.send(result);
        });
        self.cheat_rx = Some(rx);
        log::info!("cheat.sh import: {topic}");
        self.status = format!("cheat.sh: fetching {topic}...");
    }

    /// Poll the pending cheat.sh import; open the review picker with the
    /// fresh rows (already-saved ones filtered out).
    fn poll_cheat(&mut self) {
        let result = match self.cheat_rx.as_ref() {
            None => return,
            Some(rx) => match rx.try_recv() {
                Ok(r) => r,
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(_) => Err("import worker vanished".into()),
            },
        };
        self.cheat_rx = None;
        let import = match result {
            Ok(i) => i,
            Err(e) => {
                log::warn!("cheat.sh import error: {e}");
                self.status = format!("import error: {e}");
                return;
            }
        };
        if import.entries.is_empty() {
            self.status = format!("cheat.sh: no commands found for {:?}", import.topic);
            return;
        }
        let existing: std::collections::HashSet<String> = match &self.db {
            Some(db) => db.all().unwrap_or_default().into_iter().map(|c| c.command).collect(),
            None => {
                self.status = format!("import failed: no database ({})", self.db_path.display());
                return;
            }
        };
        let total = import.entries.len();
        let fresh = crate::cheatsheet::dedupe_new(import.entries, &existing);
        let dupes = total.saturating_sub(fresh.len());
        if fresh.is_empty() {
            self.status =
                format!("cheat.sh: nothing new for {:?} ({dupes} already saved)", import.topic);
            return;
        }
        self.status = if dupes > 0 {
            format!(
                "cheat.sh/{}: tick rows (space), ctrl+e edit, enter adds ({dupes} already saved)",
                import.topic
            )
        } else {
            format!("cheat.sh/{}: tick rows (space), ctrl+e edit, enter adds", import.topic)
        };
        self.cheat_picker = Some(CheatPicker::new(import.topic, fresh));
    }

    /// Keys while the import review picker is open: space ticks the row,
    /// ctrl+a ticks all shown rows, ctrl+u clears them, ctrl+e edits the
    /// highlighted row, Enter adds ticked rows, Esc cancels.
    fn handle_cheat_picker_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            self.cheat_picker = None;
            self.status = "import cancelled".into();
            return;
        }
        let ctrl = ev.mods.contains(Mods::CONTROL);
        let mut submit = false;
        let mut edit: Option<(usize, crate::cheatsheet::CheatEntry)> = None;
        if let Some(p) = self.cheat_picker.as_mut() {
            match ev.key {
                Key::Enter => submit = true,
                Key::Up => p.move_sel(-1),
                Key::Down => p.move_sel(1),
                Key::Char('p') if ctrl => p.move_sel(-1),
                Key::Char('n') if ctrl => p.move_sel(1),
                Key::Space => p.toggle_selected(),
                Key::Char('a') if ctrl => p.select_visible(true),
                Key::Char('u') if ctrl => p.select_visible(false),
                // Ctrl+e opens the two-column edit table for the highlighted
                // import row (Enter rewrites it, Esc returns here).
                Key::Char('e') if ctrl => {
                    if let Some(hit) = p.results.get(p.selected)
                        && let Some(item) = p.items.get(hit.index) {
                            edit = Some((hit.index, item.entry.clone()));
                        }
                }
                Key::Backspace => p.backspace(),
                Key::Char(_) => {
                    if let Some(t) = ev.text.as_deref().filter(|t| !t.is_empty()) {
                        for c in t.chars() {
                            p.push_char(c);
                        }
                    } else if let Key::Char(c) = ev.key {
                        p.push_char(c);
                    }
                }
                _ => {}
            }
        }
        if let Some((index, entry)) = edit {
            self.status =
                format!("editing import row #{index} (tab field, enter save, esc back)");
            self.command_form = Some(CommandForm::edit_cheat(index, &entry));
            return;
        }
        if submit {
            self.submit_cheat_import();
        }
    }

    /// Add the ticked import rows to the database, then open the command
    /// picker for a final review.
    fn submit_cheat_import(&mut self) {
        let Some(p) = self.cheat_picker.as_ref() else {
            return;
        };
        let picked = p.checked_entries();
        if picked.is_empty() {
            self.status = "tick rows with space first (ctrl+a ticks all shown)".into();
            return;
        }
        let tag = format!("cheat.sh:{}", p.topic);
        let total = picked.len();
        let mut saved = 0;
        match &self.db {
            Some(db) => {
                for e in &picked {
                    match db.add(&e.command, &e.comment, &tag) {
                        Ok(_) => saved += 1,
                        Err(err) => log::warn!("import save failed: {err}"),
                    }
                }
            }
            None => {
                self.status = format!("import failed: no database ({})", self.db_path.display());
                return;
            }
        }
        let failed = total.saturating_sub(saved);
        log::info!("cheat.sh import {}: {saved}/{total} saved", p.topic);
        self.cheat_picker = None;
        self.open_commands();
        if self.command_picker.is_some() {
            self.status = if failed > 0 {
                format!("added {saved}/{total} commands ({failed} failed)")
            } else {
                format!("added {saved} commands (tag {tag})")
            };
        }
    }

    /// Run the AI CLI on a background thread; the answer is typed into the
    /// focused pane when it arrives.
    fn ask_ai(&mut self, question: &str) {
        if self.config.ai.command.trim().is_empty() {
            self.status = "AI: set [ai] command in config.toml (e.g. \"ollama run llama3\")".into();
            return;
        }
        if self.ai_rx.is_some() {
            self.status = "AI: still thinking...".into();
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let command = self.config.ai.command.clone();
        let mut lead = self.config.ai.prompt.clone();
        // Name the exact shell the answer must be written for. An empty
        // `[general] shell` means panes run the platform default, so say
        // `cmd.exe` outright -- "default shell" makes models guess (and they
        // guess PowerShell for cmd panes).
        let shell = crate::ai::effective_shell(&self.config.general.shell);
        lead.push_str(&format!(" {}", crate::ai::shell_directive(&shell)));
        // The model queries saved commands on demand (`{"action":"db",...}`),
        // so pass a snapshot for the worker thread instead of dumping every
        // command into the first prompt.
        let saved: Vec<SavedCommand> = if self.config.ai.use_command_db {
            match &self.db {
                Some(db) => db.all().unwrap_or_default(),
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let q = question.to_string();
        // Per-turn budget for the AI CLI (clamped so a zero/typo value can
        // neither spin forever nor kill instantly).
        let timeout = std::time::Duration::from_secs(self.config.ai.timeout_s.clamp(5, 3600));
        std::thread::spawn(move || {
            let _ = tx.send(crate::ai::run_full(&command, &lead, &q, &saved, timeout));
        });
        self.ai_rx = Some(rx);
        log::info!("AI question: {question}");
        self.status = "AI: thinking... (may probe installed tools)".into();
    }

    /// Poll the pending AI request; type its command into the focused pane.
    fn poll_ai(&mut self) {
        let result = match self.ai_rx.as_ref() {
            None => return,
            Some(rx) => match rx.try_recv() {
                Ok(r) => r,
                Err(crossbeam_channel::TryRecvError::Empty) => return,
                Err(_) => Err("AI worker vanished".into()),
            },
        };
        self.ai_rx = None;
        match result {
            Ok(cmd) => {
                self.status = format!("AI: {cmd}  (press enter to run)");
                log::info!("AI suggested: {cmd}");
                let ws = self.ws_mut();
                if let Some(p) = ws.pane_mut(ws.focused) {
                    p.write(cmd.as_bytes());
                }
            }
            Err(e) => {
                log::warn!("AI error: {e}");
                self.status = format!("AI error: {e}");
            }
        }
    }

    /// Split focused leaf of current workspace; focus the new pane.
    fn split(&mut self, dir: SplitDir) {
        let shell = self.config.general.shell.clone();
        let sb = self.config.general.scrollback;
        let rt = self.rt.clone();
        match self.ws_mut().split(dir, &shell, sb, &rt) {
            Ok(id) => {
                self.status.clear();
                self.queue_fx(id, FxKind::Fresh);
                log::debug!("split {dir:?} -> pane {id}");
            }
            Err(e) => self.status = format!("split failed: {e}"),
        }
    }

    /// Close focused pane with the configured close transition.
    ///
    /// The pane stays in the tree (frozen, unfocused) while its effect plays
    /// on its own rect, then is removed at the deadline. The last pane of
    /// the last workspace quits instead of playing an effect.
    fn close_focused(&mut self) {
        self.complete_closing();
        if self.ws().leaf_ids().len() <= 1 {
            self.close_current_workspace();
            return;
        }
        let target = self.ws().focused;
        let survivor = self
            .ws()
            .leaf_ids()
            .into_iter()
            .find(|&id| id != target)
            .expect("another leaf exists");
        self.ws_mut().focused = survivor;
        match self.close_delay_ms() {
            0 => {
                self.ws_mut().remove_leaf(target);
            }
            ms => {
                self.closing.push(ClosingPane {
                    ws: self.current,
                    id: target,
                    until: std::time::Instant::now()
                        + std::time::Duration::from_millis(ms as u64),
                });
                self.queue_fx(target, FxKind::Close);
            }
        }
    }

    /// Cycle pane focus forward/backward in current workspace.
    /// While zoomed this also restores the layout first (focus still moves).
    fn cycle(&mut self, dir: i32) {
        let was_zoomed = self.ws_mut().unzoom();
        let id = self.ws_mut().cycle(dir);
        if was_zoomed {
            self.status = "pane layout restored".into();
        }
        self.queue_fx(id, FxKind::Focus);
    }

    /// Focus the nearest pane in `dir` (wezterm ActivatePaneDirection).
    /// While zoomed this restores the layout and moves in one press (the
    /// workspace falls back to tiled geometry); with nothing that way it
    /// just restores.
    fn focus_direction(&mut self, dir: Direction) {
        let was_zoomed = self.ws().zoomed().is_some();
        match self.ws_mut().focus_direction(dir) {
            Some(id) => {
                if was_zoomed {
                    self.status = "pane layout restored".into();
                }
                self.queue_fx(id, FxKind::Focus);
            }
            None if was_zoomed => {
                self.status = "pane layout restored".into();
                let id = self.ws().focused;
                self.queue_fx(id, FxKind::Focus);
            }
            None => {}
        }
    }

    /// Grow the focused pane toward `dir` (wezterm AdjustPaneSize).
    fn resize_focused(&mut self, dir: Direction) {
        self.ws_mut().resize_focused(dir, 0.05);
    }

    /// Toggle maximize on the focused pane: it takes the whole workspace
    /// area until toggled again or a focus key restores the layout.
    fn zoom_pane(&mut self) {
        if self.ws().zoomed().is_some() {
            self.ws_mut().unzoom();
            self.status = "pane layout restored".into();
            let id = self.ws().focused;
            self.queue_fx(id, FxKind::Focus);
        } else if self.ws().leaf_ids().len() < 2 {
            self.status = "zoom needs at least 2 panes".into();
        } else {
            let id = self.ws().focused;
            self.ws_mut().toggle_zoom();
            self.status = format!("pane {id} maximized (focus keys restore)");
            self.queue_fx(id, FxKind::Focus);
        }
    }

    /// Take a pending font-size change for the window host to apply.
    pub fn take_zoom_request(&mut self) -> Option<u32> {
        self.zoom_request.take()
    }

    /// Live glyph height in pixels (for the window host's font rebuilds).
    pub fn font_size(&self) -> u32 {
        self.font_size
    }

    /// Take a pending primary-font change for the window host to apply:
    /// the new `[general] font` value (empty = auto-detect).
    pub fn take_font_request(&mut self) -> Option<String> {
        self.font_request.take()
    }

    /// Font picker overlay state, mutable (rendering probes monospace flags).
    pub fn font_picker_mut(&mut self) -> Option<&mut FontPicker> {
        self.font_picker.as_mut()
    }

    /// Preview metadata for the window host: ((path, title px), w, h).
    pub fn font_preview_meta(&self) -> Option<((String, u32), u32, u32)> {
        self.font_preview
            .as_ref()
            .map(|p| (p.key.clone(), p.width, p.height))
    }

    /// Preview pixels for GPU upload. Cloned (previews are small); the
    /// retained copy lets later backend rebuilds re-upload without
    /// re-rendering.
    pub fn font_preview_pixels(&self) -> Option<Vec<u8>> {
        self.font_preview.as_ref().map(|p| p.rgba.clone())
    }

    /// Whether a preview render is in flight (spinner shows in its box).
    pub fn font_preview_loading(&self) -> bool {
        self.font_preview_loading
    }

    /// Open the font picker: scan installed fonts, fuzzy-filter overlay.
    /// Monospace flags probe lazily per visible row at render time. Starts
    /// the background preview worker and requests the first preview.
    fn open_font_picker(&mut self) {
        let fonts = crate::font::scan_fonts();
        log::info!("font picker: {} installed font(s)", fonts.len());
        self.font_preview = None;
        self.font_preview_loading = false;
        self.font_preview_sent = None;
        self.start_preview_worker();
        self.font_picker = Some(FontPicker::new(fonts, &self.config.general.font, self.font_size));
        self.status.clear();
        self.request_font_preview();
    }

    /// (Re)start the preview worker. The previous worker, if any, is
    /// abandoned: dropping its request channel makes it exit after its
    /// current render, and its generation never matches again.
    fn start_preview_worker(&mut self) {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<PreviewRequest>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded::<PreviewOutcome>();
        let spawned = std::thread::Builder::new()
            .name("font-preview".into())
            .spawn(move || {
                while let Ok(first) = req_rx.recv() {
                    // Collapse to the latest pending request: intermediate
                    // selections the user already moved past are skipped, so
                    // rapid up/down never piles up renders.
                    let mut cur = first;
                    while let Ok(newer) = req_rx.try_recv() {
                        cur = newer;
                    }
                    let PreviewRequest {
                        generation,
                        path,
                        name,
                        title_px,
                        fg,
                        bg,
                    } = cur;
                    let result = crate::font::render_preview(
                        &path,
                        &name,
                        PREVIEW_BODY,
                        title_px,
                        fg,
                        bg,
                    );
                    // A newer request that arrived mid-render wins; this stale
                    // result is dropped instead of flashing on screen.
                    if req_rx.is_empty() {
                        let _ = out_tx.send(PreviewOutcome {
                            generation,
                            path: path.to_string_lossy().into_owned(),
                            size: title_px as u32,
                            result,
                        });
                    }
                }
            })
            .inspect_err(|e| log::warn!("font preview worker failed to spawn: {e}"))
            .is_ok();
        if spawned {
            self.font_preview_tx = Some(req_tx);
            self.font_preview_rx = Some(out_rx);
        } else {
            // No worker: requests must no-op instead of piling up.
            self.font_preview_tx = None;
            self.font_preview_rx = None;
            self.font_preview_loading = false;
        }
    }

    /// Ask the worker for the highlighted font's preview, unless that exact
    /// (path, size) is already requested or shown. Sets the loading flag so
    /// the preview box shows a spinner instead of going stale-blank.
    fn request_font_preview(&mut self) {
        let Some(tx) = self.font_preview_tx.as_ref() else {
            return;
        };
        let Some(p) = self.font_picker.as_ref() else {
            return;
        };
        let Some(hit) = p.results.get(p.selected) else {
            self.font_preview = None;
            self.font_preview_loading = false;
            return;
        };
        if hit.item.path.as_os_str().is_empty() {
            // "System default": no file to render.
            self.font_preview = None;
            self.font_preview_loading = false;
            return;
        }
        let key = (
            hit.item.path.to_string_lossy().into_owned(),
            p.size,
        );
        if self.font_preview_sent.as_ref() == Some(&key) {
            return;
        }
        // Keep the old pixels on screen until the new ones land (no flicker);
        // the box title + spinner carry the loading state.
        self.font_preview_gen += 1;
        self.font_preview_sent = Some(key.clone());
        self.font_preview_loading = true;
        let theme = self.theme_colors();
        let _ = tx.send(PreviewRequest {
            generation: self.font_preview_gen,
            path: hit.item.path.clone(),
            name: hit.item.name.clone(),
            title_px: p.size as f32,
            fg: theme.fg,
            bg: theme.bg,
        });
    }

    /// Drain finished preview renders; only the latest generation applies.
    fn poll_font_preview(&mut self) {
        let mut latest: Option<PreviewOutcome> = None;
        loop {
            match self.font_preview_rx.as_ref() {
                None => return,
                Some(rx) =>                 match rx.try_recv() {
                    Ok(out) => latest = Some(out),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        // Worker gone (panic or exit): stop sending at it.
                        self.font_preview_tx = None;
                        self.font_preview_rx = None;
                        break;
                    }
                },
            }
        }
        if let Some(out) = latest {
            if out.generation != self.font_preview_gen {
                return; // Stale: the picker already moved on.
            }
            self.font_preview_loading = false;
            match out.result {
                Ok(pix) => {
                    log::debug!(
                        "font preview ready: {} ({}x{})",
                        out.path,
                        pix.width,
                        pix.height
                    );
                    self.font_preview = Some(FontPreview {
                        key: (out.path, out.size),
                        rgba: pix.rgba,
                        width: pix.width,
                        height: pix.height,
                    });
                }
                Err(e) => {
                    log::debug!("font preview failed: {e}");
                    self.font_preview = None;
                }
            }
        }
    }

    /// Stop the preview worker and forget pending state.
    fn drop_preview_worker(&mut self) {
        self.font_preview_tx = None;
        self.font_preview_rx = None;
        self.font_preview_loading = false;
        self.font_preview_sent = None;
    }

    /// Keys while the font picker is open (modal). Tab switches between the
    /// font list and the size box; the size previews live in the terminal.
    /// Enter applies font + size, Esc restores the entry state and closes.
    fn handle_font_picker_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            self.cancel_font_picker();
            return;
        }
        if matches!(ev.key, Key::Tab | Key::BackTab) {
            if let Some(p) = self.font_picker.as_mut() {
                p.focus = match p.focus {
                    PickerFocus::List => PickerFocus::Size,
                    PickerFocus::Size => PickerFocus::List,
                };
                p.size_buf = None;
            }
            return;
        }
        let ctrl = ev.mods.contains(Mods::CONTROL);
        let mut pick: Option<(FontEntry, Option<bool>)> = None;
        if let Some(p) = self.font_picker.as_mut() {
            match p.focus {
                PickerFocus::Size => match ev.key {
                    Key::Enter => {
                        pick = p
                            .results
                            .get(p.selected)
                            .map(|h| (h.item.clone(), h.mono))
                    }
                    Key::Up | Key::Char('k') | Key::Char('+') | Key::Char('=') => {
                        p.nudge_size(1)
                    }
                    Key::Down | Key::Char('j') | Key::Char('-') | Key::Char('_') => {
                        p.nudge_size(-1)
                    }
                    Key::Backspace => p.size_backspace(),
                    Key::Char(c) if c.is_ascii_digit() => p.push_digit(c),
                    _ => {}
                },
                PickerFocus::List => match ev.key {
                    Key::Enter => {
                        pick = p
                            .results
                            .get(p.selected)
                            .map(|h| (h.item.clone(), h.mono))
                    }
                    Key::Up => p.move_sel(-1),
                    Key::Down => p.move_sel(1),
                    Key::Char('p') if ctrl => p.move_sel(-1),
                    Key::Char('n') if ctrl => p.move_sel(1),
                    Key::Backspace => p.backspace(),
                    Key::Char(_) | Key::Space => {
                        if let Some(t) = ev.text.as_deref().filter(|t| !t.is_empty()) {
                            for c in t.chars() {
                                p.push_char(c);
                            }
                        } else if let Key::Char(c) = ev.key {
                            p.push_char(c);
                        } else {
                            p.push_char(' ');
                        }
                    }
                    _ => {}
                },
            }
        }
        // Live size preview, like the zoom keys (backend rebuilds).
        let live = self
            .font_picker
            .as_ref()
            .map(|p| p.size)
            .unwrap_or(self.font_size);
        if live != self.font_size {
            self.font_size = live;
            self.zoom_request = Some(live);
        }
        // Queue an async preview of the newly highlighted font/size unless
        // this keypress is about to close the picker anyway.
        if pick.is_none() {
            self.request_font_preview();
        }
        if let Some((entry, mono)) = pick {
            self.pick_font(entry, mono);
        }
    }

    /// Close the picker, restoring the font + size live at open. The config
    /// file is untouched (it only changes on Enter).
    fn cancel_font_picker(&mut self) {
        if let Some(p) = self.font_picker.take() {
            self.config.general.font = p.orig_font;
            if self.font_size != p.orig_size {
                self.font_size = p.orig_size;
                self.zoom_request = Some(p.orig_size);
            }
            // The backend still runs the entry font: selection previews never
            // apply, so no font rebuild is needed.
        }
        self.drop_preview_worker();
        self.font_preview = None;
        self.status.clear();
    }

    /// Apply a picked font: validate, update `[general] font` (+ size) in
    /// memory and on disk, and queue a live rebuild for the window host.
    /// `mono` is the picker's cached probe (re-probed when unknown).
    fn pick_font(&mut self, entry: FontEntry, mono: Option<bool>) {
        let size = self
            .font_picker
            .as_ref()
            .map(|p| p.size)
            .unwrap_or(self.font_size);
        self.font_picker = None;
        self.drop_preview_worker();
        self.font_preview = None;
        if !entry.path.as_os_str().is_empty() {
            if !crate::font::is_font_file(&entry.path) {
                self.status = format!("font: not a font file: {}", entry.path.display());
                return;
            }
            if let Err(e) = std::fs::read(&entry.path) {
                self.status = format!("font: cannot read {}: {e}", entry.path.display());
                return;
            }
        }
        let value = entry.path.to_string_lossy().into_owned();
        self.config.general.font = value.clone();
        self.config.general.font_size = size;
        // Persist so hot reload (and the next launch) keeps both.
        let saved = match self.config.write_font_settings() {
            Ok(path) => format!(" (saved to {})", path.display()),
            Err(e) => format!(" (config not saved: {e})"),
        };
        self.font_request = Some(value);
        let shown = if entry.path.as_os_str().is_empty() {
            "system default".to_string()
        } else {
            entry.name.clone()
        };
        // A proportional pick still applies on request, but say so loudly:
        // varying glyph advances misalign the terminal grid.
        let mono = match mono {
            Some(m) => Some(m),
            None if entry.path.as_os_str().is_empty() => None,
            None => crate::font::is_monospaced(&entry.path).ok(),
        };
        let mono_warn = match mono {
            Some(false) => " (warning: not a monospace font, expect misalignment)",
            _ => "",
        };
        log::info!("font picked: {shown} {size}px{saved}{mono_warn}");
        self.status = format!("font: {shown} {size}px{saved}{mono_warn}");
    }

    /// Step the font size, requesting a backend rebuild from the host.
    fn zoom_by(&mut self, delta: i32) {
        let next = (self.font_size as i32 + delta).clamp(6, 48) as u32;
        if next != self.font_size {
            self.font_size = next;
            self.zoom_request = Some(next);
            self.status = format!("font {next}px (ctrl+0 resets)");
        }
    }

    /// Reset the font size to the configured value.
    fn zoom_reset(&mut self) {
        let base = self.config.general.font_size.max(6);
        if base != self.font_size {
            self.font_size = base;
            self.zoom_request = Some(base);
        }
        self.status = format!("font {base}px");
    }

    /// Rebuild the top menu's items so the bar always lists workspaces and
    /// actions, even before the inbox is opened. Skipped while open so
    /// navigation state is preserved.
    pub fn sync_menu(&mut self) {
        if self.menu_open {
            return;
        }
        self.build_menu();
    }

    /// Build menu items (workspaces with live status + actions).
    fn build_menu(&mut self) {
        let mut ws_items: Vec<MenuItem<WsAction>> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let cur = if i == self.current { ">" } else { " " };
                MenuItem::item(
                    format!("{cur}{}: {} [{}]", i + 1, w.name, w.status_line()),
                    WsAction::Switch(i),
                )
            })
            .collect();
        if ws_items.is_empty() {
            ws_items.push(MenuItem::item("(no workspace)", WsAction::New));
        }
        self.menu = MenuState::new(vec![
            MenuItem::group("Workspaces", ws_items),
            MenuItem::group(
                "Actions",
                vec![
                    MenuItem::item("New workspace", WsAction::New),
                    MenuItem::item("Close current workspace", WsAction::CloseCurrent),
                    MenuItem::item("Rename current workspace", WsAction::RenameCurrent),
                ],
            ),
            MenuItem::group(
                "Commands",
                vec![
                    MenuItem::item("Import commands from cheat.sh…", WsAction::ImportCheat),
                    MenuItem::item("Saved commands (pick & run)", WsAction::SavedCommands),
                    MenuItem::item("Add command…", WsAction::NewCommand),
                ],
            ),
        ]);
    }

    /// Open the inbox dropdown (ctrl+o).
    fn open_inbox(&mut self) {
        self.build_menu();
        // Highlight the first group and expand it, so the dropdown is
        // visible immediately instead of only after a navigation key, then
        // walk down to the current workspace row.
        self.menu.activate();
        self.menu.select();
        for _ in 0..self.current {
            self.menu.down();
        }
        self.menu_open = true;
        self.status = "menu: h/l or arrows pick, J/K move workspace, enter select, esc close".into();
        log::info!("inbox opened ({} workspaces)", self.workspaces.len());
    }

    /// Inbox menu navigation (modal: consumes all keys while open).
    fn handle_menu_key(&mut self, ev: KeyPress) {
        // J/K (or shift+up/down) reorder the highlighted workspace instead
        // of navigating; the menu stays open on the moved row.
        let shift = ev.mods.contains(Mods::SHIFT);
        let reorder: Option<i32> = match ev.key {
            Key::Char('J') => Some(1),
            Key::Char('K') => Some(-1),
            Key::Down if shift => Some(1),
            Key::Up if shift => Some(-1),
            _ => None,
        };
        if let Some(dir) = reorder {
            self.move_highlighted_workspace(dir);
            return;
        }
        let mut close = false;
        let events = {
            let menu = &mut self.menu;
            match ev.key {
                Key::Left | Key::Char('h') => menu.left(),
                Key::Right | Key::Char('l') => menu.right(),
                Key::Up | Key::Char('k') => menu.up(),
                Key::Down | Key::Char('j') => menu.down(),
                Key::Enter => menu.select(),
                Key::Esc => close = true,
                _ => {}
            }
            menu.drain_events().collect::<Vec<_>>()
        };
        if let Some(MenuEvent::Selected(action)) = events.into_iter().next() {
            self.menu.reset();
            self.menu_open = false;
            self.apply_menu_action(action);
            return;
        }
        if close {
            self.menu.reset();
            self.menu_open = false;
        }
    }

    /// Move the highlighted workspace row one step (`dir`: +1 down, -1 up).
    /// Only workspace rows move; group headers and action rows are ignored.
    /// The menu is rebuilt with the moved row highlighted so repeated moves
    /// keep working without reopening.
    fn move_highlighted_workspace(&mut self, dir: i32) {
        let Some(WsAction::Switch(i)) = self.menu.highlight().and_then(|item| item.data.clone())
        else {
            return;
        };
        let n = self.workspaces.len();
        if n == 0 {
            return;
        }
        let to = (i as i32 + dir).clamp(0, n.saturating_sub(1) as i32) as usize;
        if to == i {
            return;
        }
        self.move_workspace(i, to);
        // Rebuild around the moved row: same open state as `open_inbox`,
        // then walk down to its new position.
        self.build_menu();
        self.menu.activate();
        self.menu.select();
        for _ in 0..to {
            self.menu.down();
        }
        self.menu_open = true;
        self.status = format!(
            "moved workspace {} → {} (J/K move, enter select, esc close)",
            i + 1,
            to + 1
        );
    }

    /// Apply one inbox action.
    fn apply_menu_action(&mut self, action: WsAction) {
        match action {
            WsAction::Switch(i) => self.switch_to(i),
            WsAction::New => self.new_workspace(),
            WsAction::CloseCurrent => self.close_current_workspace(),
            WsAction::RenameCurrent => {
                self.prompt = Some(Prompt::open(PromptKind::RenameWorkspace));
            }
            WsAction::SavedCommands => self.open_commands(),
            WsAction::NewCommand => self.open_command_form(),
            WsAction::ImportCheat => {
                self.prompt = Some(Prompt::open(PromptKind::CheatSheet));
            }
        }
    }

    /// Palette state (for rendering).
    pub fn palette_ref(&self) -> Option<&Palette> {
        self.palette.as_ref()
    }

    /// Open the command palette (ctrl+p).
    fn open_palette(&mut self) {
        self.palette = Some(Palette::new());
        self.status.clear();
    }

    /// Keys while the palette is open (modal): type to fuzzy-filter, arrows
    /// or ctrl+n/p to move, Enter to run, Esc to close.
    fn handle_palette_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            self.palette = None;
            self.status.clear();
            return;
        }
        let ctrl = ev.mods.contains(Mods::CONTROL);
        let mut run: Option<Command> = None;
        let Some(p) = self.palette.as_mut() else {
            return;
        };
        match ev.key {
            Key::Enter => run = p.results.get(p.selected).map(|i| i.cmd),
            Key::Up => p.move_sel(-1),
            Key::Down => p.move_sel(1),
            Key::Char('p') if ctrl => p.move_sel(-1),
            Key::Char('n') if ctrl => p.move_sel(1),
            Key::Backspace => p.backspace(),
            // Any other printable key (Honor the produced text when present.)
            Key::Char(_) | Key::Space => {
                if let Some(t) = ev.text.as_deref().filter(|t| !t.is_empty()) {
                    for c in t.chars() {
                        p.push_char(c);
                    }
                } else if let Key::Char(c) = ev.key {
                    p.push_char(c);
                } else {
                    p.push_char(' ');
                }
            }
            _ => {}
        }
        if let Some(cmd) = run {
            self.palette = None;
            self.status.clear();
            self.run_command(cmd);
        }
    }

    /// Saved-command picker/form/args accessors (for rendering).
    pub fn command_picker_ref(&self) -> Option<&CommandPicker> {
        self.command_picker.as_ref()
    }

    /// cheat.sh import review overlay (for rendering).
    pub fn cheat_picker_ref(&self) -> Option<&CheatPicker> {
        self.cheat_picker.as_ref()
    }

    pub fn command_form_mut(&mut self) -> Option<&mut CommandForm> {
        self.command_form.as_mut()
    }

    pub fn command_args_mut(&mut self) -> Option<&mut CommandArgs> {
        self.command_args.as_mut()
    }

    /// Open the saved-command picker (ctrl+r), loading the database fresh.
    fn open_commands(&mut self) {
        let Some(db) = &self.db else {
            self.status = format!("commands: database unavailable ({})", self.db_path.display());
            return;
        };
        let path = db.path().display().to_string();
        match db.all() {
            Ok(all) => {
                if all.is_empty() {
                    self.status =
                        format!("commands: none saved yet (ctrl+shift+r to add) [{path}]");
                } else {
                    self.status.clear();
                }
                self.command_picker = Some(CommandPicker::new(all));
            }
            Err(e) => self.status = format!("commands: {e}"),
        }
    }

    /// Keys while the saved-command picker is open.
    fn handle_command_picker_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            self.command_picker = None;
            self.status.clear();
            return;
        }
        let ctrl = ev.mods.contains(Mods::CONTROL);
        let mut pick: Option<SavedCommand> = None;
        let mut delete: Option<i64> = None;
        let mut edit: Option<SavedCommand> = None;
        if let Some(p) = self.command_picker.as_mut() {
            match ev.key {
                Key::Enter => pick = p.results.get(p.selected).map(|h| h.item.clone()),
                Key::Up => p.move_sel(-1),
                Key::Down => p.move_sel(1),
                Key::Char('p') if ctrl => p.move_sel(-1),
                Key::Char('n') if ctrl => p.move_sel(1),
                // Ctrl+e opens the two-column edit table for the highlighted
                // row (Enter there updates it in place, Esc returns here).
                Key::Char('e') if ctrl => {
                    edit = p.results.get(p.selected).map(|h| h.item.clone())
                }
                Key::Delete => delete = p.results.get(p.selected).map(|h| h.item.id),
                Key::Backspace => p.backspace(),
                Key::Char(_) | Key::Space => {
                    if let Some(t) = ev.text.as_deref().filter(|t| !t.is_empty()) {
                        for c in t.chars() {
                            p.push_char(c);
                        }
                    } else if let Key::Char(c) = ev.key {
                        p.push_char(c);
                    } else {
                        p.push_char(' ');
                    }
                }
                _ => {}
            }
        }
        if let Some(item) = edit {
            self.command_picker = None;
            self.status = format!(
                "editing command #{} (tab field, enter save, esc back)",
                item.id
            );
            self.command_form = Some(CommandForm::edit_command(&item));
            return;
        }
        if let Some(id) = delete {
            if let Some(db) = &self.db
                && let Err(e) = db.delete(id) {
                    self.status = format!("delete failed: {e}");
                }
            if let Some(p) = self.command_picker.as_mut() {
                p.remove_id(id);
            }
            if !self.status.starts_with("delete failed") {
                self.status = format!("deleted command #{id}");
            }
        }
        if let Some(item) = pick {
            self.pick_saved(item);
        }
    }

    /// Run a picked command, or ask for placeholder values first.
    fn pick_saved(&mut self, item: SavedCommand) {
        self.command_picker = None;
        self.status.clear();
        if commands_db::placeholders(&item.command).is_empty() {
            self.run_saved(item.id, item.command);
        } else {
            self.command_args = Some(CommandArgs::new(item.id, item.command));
        }
    }

    /// Type a saved command into the focused pane and run it.
    fn run_saved(&mut self, id: i64, text: String) {
        if let Some(db) = &self.db
            && let Err(e) = db.mark_used(id) {
                log::warn!("command db mark_used: {e}");
            }
        log::info!("command run: {text}");
        self.run_in_pane(&text);
        self.status = format!("run: {text}");
    }

    /// Type `text` into the focused pane, then press Enter.
    fn run_in_pane(&mut self, text: &str) {
        let ws = self.ws_mut();
        if let Some(p) = ws.pane_mut(ws.focused) {
            p.write(text.as_bytes());
            p.write(b"\r");
        }
    }

    /// Open the "save command" form (ctrl+shift+r).
    fn open_command_form(&mut self) {
        self.command_form = Some(CommandForm::new());
        self.status.clear();
    }

    /// Keys while the save/edit form is open: Tab switches field, Enter
    /// validates, Esc goes back (to the picker that opened it, if any).
    fn handle_command_form_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            let origin = self.command_form.as_ref().map(|f| f.origin);
            self.command_form = None;
            self.status.clear();
            // The picker that opened the form is still live underneath
            // (mode() puts CommandForm first), so it simply reappears.
            match origin {
                Some(FormOrigin::CommandPicker) | Some(FormOrigin::CheatPicker)
                    // Refresh the command picker from the DB in case a prior
                    // edit changed it.
                    if origin == Some(FormOrigin::CommandPicker)
                        && self.command_picker.is_none()
                    => {
                        self.open_commands();
                    }
                _ => {}
            }
            return;
        }
        let mut save = false;
        if let Some(f) = self.command_form.as_mut() {
            match ev.key {
                Key::Tab => f.field = (f.field + 1) % 2,
                Key::BackTab => f.field = (f.field + 1) % 2,
                Key::Enter => save = !f.command_text().is_empty(),
                _ => {
                    let input = textarea_input(&ev);
                    f.active_mut().input(input);
                }
            }
        }
        if save {
            self.save_command();
        }
    }

    /// Persist the form's command + comment, then close. A command edit
    /// updates the DB row (tags and use counter kept); a cheat edit
    /// rewrites the pending import row; a fresh form inserts.
    fn save_command(&mut self) {
        let Some(form) = self.command_form.take() else {
            return;
        };
        let command = form.command_text();
        if command.is_empty() {
            self.status = "save: command is empty".into();
            self.command_form = Some(form);
            return;
        }
        let comment = form.comment_text();
        if let Some(index) = form.edit_cheat {
            // Rewrite the pending cheat.sh row in place; nothing is saved
            // to the database until the import is confirmed.
            if let Some(p) = self.cheat_picker.as_mut()
                && let Some(item) = p.items.get_mut(index) {
                    item.entry.command = command.clone();
                    item.entry.comment = comment;
                }
            self.status = format!("edited import row: {command}");
            return;
        }
        // Do the DB write first, then (for an edit) refresh the picker so
        // the changed row shows; set the status last so the refresh's clear
        // does not wipe the message.
        let outcome: Result<String, String> = match &self.db {
            Some(db) => match form.edit_id {
                Some(id) => db
                    .update(id, &command, &comment)
                    .map(|_| format!("updated command #{id}: {command}"))
                    .map_err(|e| format!("update failed: {e}")),
                None => db
                    .add(&command, &comment, "")
                    .map(|id| format!("saved command #{id}: {command}"))
                    .map_err(|e| format!("save failed: {e}")),
            },
            None => Err("save failed: no database".into()),
        };
        match outcome {
            Ok(msg) => {
                log::info!("{msg}");
                if form.edit_id.is_some() && form.origin == FormOrigin::CommandPicker {
                    self.open_commands();
                }
                self.status = msg;
            }
            Err(e) => self.status = e,
        }
    }

    /// Keys while filling placeholder values: Enter/Tab advance (or submit),
    /// Esc cancels.
    fn handle_command_args_key(&mut self, ev: KeyPress) {
        if matches!(ev.key, Key::Esc) {
            self.command_args = None;
            self.status.clear();
            return;
        }
        let mut submit = false;
        if let Some(a) = self.command_args.as_mut() {
            match ev.key {
                Key::Enter | Key::Tab => {
                    if a.index + 1 < a.fields.len() {
                        a.index += 1;
                    } else {
                        submit = true;
                    }
                }
                Key::BackTab => a.index = a.index.saturating_sub(1),
                _ => {
                    let input = textarea_input(&ev);
                    if let Some(f) = a.fields.get_mut(a.index) {
                        f.input(input);
                    }
                }
            }
        }
        if submit {
            self.submit_command_args();
        }
    }

    /// Fill the template with the entered values and run it.
    fn submit_command_args(&mut self) {
        let Some(a) = self.command_args.take() else {
            return;
        };
        let text = commands_db::fill(&a.template, &a.values());
        self.run_saved(a.id, text);
    }

    /// Execute a command (from a key binding, the palette, or a macro).
    fn run_command(&mut self, cmd: Command) {
        match cmd {
            Command::SplitHorizontal => self.split(SplitDir::Horizontal),
            Command::SplitVertical => self.split(SplitDir::Vertical),
            Command::ClosePane => self.close_focused(),
            Command::OpenMcpSplitH => self.open_mcp_pane(Some(SplitDir::Horizontal)),
            Command::OpenMcpSplitV => self.open_mcp_pane(Some(SplitDir::Vertical)),
            Command::OpenMcpHere => self.open_mcp_pane(None),
            Command::ShareTerminal => self.share_focused(),
            Command::StopSharing => self.stop_sharing(self.ws().focused),
            Command::ZoomPane => self.zoom_pane(),
            Command::FocusNext => self.cycle(1),
            Command::FocusPrev => self.cycle(-1),
            Command::FocusLeft => self.focus_direction(Direction::Left),
            Command::FocusRight => self.focus_direction(Direction::Right),
            Command::FocusUp => self.focus_direction(Direction::Up),
            Command::FocusDown => self.focus_direction(Direction::Down),
            Command::ResizeLeft => self.resize_focused(Direction::Left),
            Command::ResizeRight => self.resize_focused(Direction::Right),
            Command::ResizeUp => self.resize_focused(Direction::Up),
            Command::ResizeDown => self.resize_focused(Direction::Down),
            Command::NewWorkspace => self.new_workspace(),
            Command::CloseWorkspace => self.close_current_workspace(),
            Command::RenameWorkspace => {
                self.prompt = Some(Prompt::open(PromptKind::RenameWorkspace));
            }
            Command::WorkspaceNext => self.switch_relative(1),
            Command::WorkspacePrev => self.switch_relative(-1),
            Command::Inbox => self.open_inbox(),
            Command::Scrollback => self.toggle_scrollback(),
            Command::Image => {
                // Ask the shell for its cwd first; the reply refreshes the
                // completion (see `poll_cwd_reply`).
                self.request_cwd();
                self.prompt = Some(Prompt::open(PromptKind::ViewImage));
                self.update_prompt_ghost();
            }
            Command::Paste => self.paste_clipboard(),
            Command::CommandPalette => self.open_palette(),
            Command::Commands => self.open_commands(),
            Command::SaveCommand => self.open_command_form(),
            Command::SaveLayout => {
                self.status = match self.save_layout() {
                    Ok(()) => format!("layout saved to {}", self.layout_path.display()),
                    Err(e) => format!("layout save failed: {e}"),
                };
            }
            Command::YankLast => self.yank_last_output(),
            Command::SelectMode => self.toggle_select(),
            Command::EditConfig => self.edit_config(),
            Command::ZoomIn => self.zoom_by(1),
            Command::ZoomOut => self.zoom_by(-1),
            Command::ZoomReset => self.zoom_reset(),
            Command::ChangeFont => self.open_font_picker(),
            Command::AskAi => self.prompt = Some(Prompt::open(PromptKind::AskAi)),
            Command::ImportCheat => {
                self.prompt = Some(Prompt::open(PromptKind::CheatSheet));
            }
            Command::Help => self.about = !self.about,
            Command::Quit => self.should_quit = true,
        }
    }

    /// Ask the focused shell for its current directory (async reply).
    /// Records the starting generation so the reply can trigger a refresh.
    fn request_cwd(&mut self) {
        let fid = self.ws().focused;
        let seq = self.ws().pane(fid).map(|p| p.cwd_seq());
        if let Some(seq) = seq {
            self.cwd_seq_seen = seq;
        }
        if let Some(p) = self.ws_mut().pane_mut(fid) {
            p.request_cwd();
        }
    }

    /// Paste the system clipboard into the focused pane.
    fn paste_clipboard(&mut self) {
        let text = match crate::clipboard::paste() {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("paste failed: {e}");
                return;
            }
        };
        let ws = self.ws_mut();
        if let Some(p) = ws.pane_mut(ws.focused) {
            p.paste(&text);
        }
        self.status = format!("pasted {} chars", text.chars().count());
    }

    /// Focus the pane containing cell `(col, row)` (mouse click). Returns true
    /// when a pane took focus.
    pub fn focus_pane_at(&mut self, col: u16, row: u16) -> bool {
        if let Some(id) = self.ws().pane_at(col, row) {
            if id != self.ws().focused {
                self.ws_mut().focused = id;
                self.queue_fx(id, FxKind::Focus);
            }
            true
        } else {
            false
        }
    }

    /// Scroll the focused pane's scrollback by `delta` rows.
    pub fn scroll_focused(&mut self, delta: i32) {
        let ws = self.ws_mut();
        if let Some(p) = ws.pane_mut(ws.focused) {
            p.scroll(delta);
        }
    }

    /// Modifier gating modifier+click URL opening, from `[mouse] url_mod`
    /// ("ctrl" default; "alt"/"shift" work, anything else falls back to
    /// ctrl so plain clicks never open links by accident).
    fn url_mods(&self) -> Mods {
        match self.config.mouse.url_mod.trim().to_ascii_lowercase().as_str() {
            "alt" => Mods::ALT,
            "shift" => Mods::SHIFT,
            _ => Mods::CONTROL,
        }
    }

    /// Grid cell under full-area `(col, row)`: pane id plus inner-grid
    /// coords. Clicks on borders return None (nothing selectable there).
    fn inner_at(&self, col: u16, row: u16) -> Option<(usize, u16, u16)> {
        let ws = self.ws();
        let id = ws.pane_at(col, row)?;
        let rect = ws.pane_rect(id)?;
        let inner = Block::default().borders(Borders::ALL).inner(rect);
        if col < inner.x || row < inner.y {
            return None;
        }
        let (c, r) = (col - inner.x, row - inner.y);
        if c >= inner.width || r >= inner.height {
            return None;
        }
        Some((id, c, r))
    }

    /// URL under full-area `(col, row)`, if any.
    fn url_at(&self, col: u16, row: u16) -> Option<String> {
        let (id, c, r) = self.inner_at(col, row)?;
        let lines = self.ws().pane(id)?.grid_lines();
        crate::mouse::find_url(&lines, r as usize, c as usize)
    }

    /// Top-level menu groups, in bar order. Must stay in sync with
    /// [`App::build_menu`]; the bar renders ` {name} ` per group after one
    /// leading space (see `tui-menu`'s `Menu` widget).
    const MENU_GROUPS: [&'static str; 3] = ["Workspaces", "Actions", "Commands"];

    /// Start column (0-based) of menu group `g` in the top bar.
    fn menu_group_start(group: usize) -> u16 {
        let mut x: u16 = 1; // leading space like `tui-menu`
        for name in Self::MENU_GROUPS.iter().take(group) {
            x = x.saturating_add(name.chars().count() as u16 + 2);
        }
        x
    }

    /// Menu group under `col` on the top bar row, if any.
    fn menubar_group_at(col: u16) -> Option<usize> {
        for (g, name) in Self::MENU_GROUPS.iter().enumerate() {
            let start = Self::menu_group_start(g);
            let w = name.chars().count() as u16 + 2; // ` {name} `
            if col >= start && col < start.saturating_add(w) {
                return Some(g);
            }
        }
        None
    }

    /// Dropdown labels for group `g`, mirroring [`App::build_menu`] so
    /// hit-testing matches what `tui-menu` renders.
    fn menu_group_labels(&self, group: usize) -> Vec<String> {
        match group {
            0 => {
                let labels: Vec<String> = self
                    .workspaces
                    .iter()
                    .enumerate()
                    .map(|(i, w)| {
                        let cur = if i == self.current { ">" } else { " " };
                        format!("{cur}{}: {} [{}]", i + 1, w.name, w.status_line())
                    })
                    .collect();
                if labels.is_empty() {
                    vec!["(no workspace)".to_string()]
                } else {
                    labels
                }
            }
            1 => vec![
                "New workspace".to_string(),
                "Close current workspace".to_string(),
                "Rename current workspace".to_string(),
            ],
            2 => vec![
                "Import commands from cheat.sh…".to_string(),
                "Saved commands (pick & run)".to_string(),
                "Add command…".to_string(),
            ],
            _ => Vec::new(),
        }
    }

    /// Dropdown rect `(x, y, w, h)` for group `g`, mirroring `tui-menu`'s
    /// `render_dropdown`: origin `(group_x, bar_y + 1)`, width
    /// `max_label + 6` (border + padding), height `items + 2` (border).
    fn dropdown_rect(&self, group: usize) -> Option<(u16, u16, u16, u16)> {
        let labels = self.menu_group_labels(group);
        if labels.is_empty() {
            return None;
        }
        let max_w = labels
            .iter()
            .map(|l| l.chars().count() as u16)
            .max()
            .unwrap_or(0);
        let w = max_w.saturating_add(6).max(4);
        let h = labels.len() as u16 + 2;
        Some((Self::menu_group_start(group), 1, w, h))
    }

    /// Dropdown item under `(col, row)` for the currently open group, if
    /// any. Only the highlighted group's dropdown is rendered by
    /// `tui-menu`, so only it is clickable.
    fn dropdown_hit(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        if !self.menu_open {
            return None;
        }
        let group = self.menu_dropdown_group()?;
        let (x, y, w, h) = self.dropdown_rect(group)?;
        if col < x || col >= x.saturating_add(w) {
            return None;
        }
        if row < y || row >= y.saturating_add(h) {
            return None;
        }
        // Items live on rows `y+1 .. y+len` (inside the border).
        let len = self.menu_group_labels(group).len() as u16;
        if row < y + 1 || row >= y + 1 + len {
            return None;
        }
        Some((group, (row - (y + 1)) as usize))
    }

    /// Which group's dropdown is currently rendered: derived from the
    /// highlighted action, since our actions are partitioned by group
    /// (`Switch` = Workspaces, `New/CloseCurrent/RenameCurrent` = Actions,
    /// the rest = Commands).
    fn menu_dropdown_group(&self) -> Option<usize> {
        match self.menu.highlight().and_then(|item| item.data.clone()) {
            Some(WsAction::Switch(_)) => Some(0),
            Some(WsAction::New | WsAction::CloseCurrent | WsAction::RenameCurrent) => Some(1),
            Some(WsAction::SavedCommands | WsAction::NewCommand | WsAction::ImportCheat) => Some(2),
            None => None,
        }
    }

    /// Open the menu on group `g` (bar click): same state as
    /// [`App::open_inbox`] but for an arbitrary group.
    fn open_menu_group(&mut self, group: usize) {
        self.build_menu();
        self.menu.activate();
        for _ in 0..group.min(Self::MENU_GROUPS.len().saturating_sub(1)) {
            self.menu.right();
        }
        self.menu.select();
        self.menu_open = true;
        self.status = "menu: click an item to select, esc to close".into();
        log::info!("menu opened on group {group} via mouse");
    }

    /// Activate dropdown item `item` in group `group` (mouse click): walk
    /// the highlight down to the row, then select it like Enter would.
    fn click_dropdown_item(&mut self, group: usize, item: usize) {
        self.build_menu();
        self.menu.activate();
        for _ in 0..group.min(Self::MENU_GROUPS.len().saturating_sub(1)) {
            self.menu.right();
        }
        self.menu.select();
        for _ in 0..item {
            self.menu.down();
        }
        self.menu.select();
        let events: Vec<MenuEvent<WsAction>> = self.menu.drain_events().collect();
        if let Some(MenuEvent::Selected(action)) = events.into_iter().next() {
            self.menu.reset();
            self.menu_open = false;
            self.apply_menu_action(action);
            return;
        }
        // Clicked row had no action (should not happen): keep it open.
        self.menu_open = true;
    }

    /// Button press/release from the window host. The menu bar (row 0) and
    /// the open dropdown always win: left-press there opens/switches the
    /// menu or activates an item and returns [`MouseClickOutcome::Menu`]
    /// (the caller must skip focus + forwarding for that click). A left
    /// press anywhere else while the menu is open dismisses it (and focuses
    /// the pane under the cursor) instead of driving the pane.
    /// Otherwise modifier+click on a link opens it, and presses go to panes
    /// whose app enabled mouse tracking; the caller keeps its legacy
    /// click-to-focus behavior for the rest.
    pub fn mouse_button(
        &mut self,
        col: u16,
        row: u16,
        button: crate::mouse::MouseButton,
        pressed: bool,
        mods: Mods,
    ) -> MouseClickOutcome {
        // Any button activity starts a fresh gesture: drop the motion
        // coalescing cache so the next move reports even at the same cell.
        self.last_mouse_cell = None;
        // Menu bar clicks work even with `[mouse] enabled = false`: the
        // switch only gates forwarding to terminal apps.
        if pressed && button == crate::mouse::MouseButton::Left {
            if row == 0 {
                if let Some(group) = Self::menubar_group_at(col) {
                    // Clicking the open group toggles the menu shut.
                    if self.menu_open && self.menu_dropdown_group() == Some(group) {
                        self.menu.reset();
                        self.menu_open = false;
                        self.status.clear();
                    } else {
                        self.open_menu_group(group);
                    }
                    return MouseClickOutcome::Menu;
                }
                // Click on bar padding: dismiss an open menu, else ignore.
                if self.menu_open {
                    self.menu.reset();
                    self.menu_open = false;
                    self.status.clear();
                    return MouseClickOutcome::Menu;
                }
                return MouseClickOutcome::Ignored;
            }
            if self.menu_open {
                if let Some((group, item)) = self.dropdown_hit(col, row) {
                    self.click_dropdown_item(group, item);
                    return MouseClickOutcome::Menu;
                }
                // Click outside the menu dismisses it and focuses the pane
                // below (no bytes go to the app for this click).
                self.menu.reset();
                self.menu_open = false;
                self.status.clear();
                self.focus_pane_at(col, row);
                return MouseClickOutcome::Menu;
            }
        } else if self.menu_open {
            // Releases / non-left buttons never reach panes while open.
            return MouseClickOutcome::Menu;
        }
        if !self.config.mouse.enabled || self.mode() != Mode::Normal {
            return MouseClickOutcome::Ignored;
        }
        if pressed && mods.contains(self.url_mods())
            && let Some(url) = self.url_at(col, row) {
                if crate::mouse::is_openable_url(&url) {
                    log::info!("open url: {url}");
                    match open::that(&url) {
                        Ok(()) => self.status = format!("opened {url}"),
                        Err(e) => self.status = format!("open failed: {e}"),
                    }
                    return MouseClickOutcome::OpenedUrl;
                }
                return MouseClickOutcome::Ignored;
            }
        let Some((id, c, r)) = self.inner_at(col, row) else {
            return MouseClickOutcome::Ignored;
        };
        let state = match self.ws().pane(id) {
            Some(pane) => pane.mouse_state(),
            None => return MouseClickOutcome::Ignored,
        };
        if !state.tracking {
            return MouseClickOutcome::Ignored;
        }
        let bytes = if pressed {
            crate::mouse::encode_press(button, c, r, mods, state.encoding)
        } else {
            if !state.release {
                return MouseClickOutcome::Ignored;
            }
            crate::mouse::encode_release(button, c, r, mods, state.encoding)
        };
        if let Some(pane) = self.ws_mut().pane_mut(id) {
            pane.write(&bytes);
            MouseClickOutcome::Forwarded
        } else {
            MouseClickOutcome::Ignored
        }
    }

    /// Pointer motion from the window host (`held` = left button is down).
    /// Only forwarded when the pane under the cursor asked for it, and only
    /// when the target cell (or button state) actually changed: a stationary
    /// or jittering pointer inside one cell sends nothing, so holding the
    /// button no longer streams repeated reports. Returns whether anything
    /// was written.
    pub fn mouse_move(&mut self, col: u16, row: u16, mods: Mods, held: bool) -> bool {
        if !self.config.mouse.enabled || self.mode() != Mode::Normal {
            return false;
        }
        let Some((id, c, r)) = self.inner_at(col, row) else {
            self.last_mouse_cell = None;
            return false;
        };
        let state = match self.ws().pane(id) {
            Some(pane) => pane.mouse_state(),
            None => {
                self.last_mouse_cell = None;
                return false;
            }
        };
        if !state.wants_motion(held) {
            return false;
        }
        // Coalesce: identical consecutive reports carry no new information
        // (the app only sees cell coordinates), so drop them.
        let key = (id, c, r, held);
        if self.last_mouse_cell == Some(key) {
            return false;
        }
        self.last_mouse_cell = Some(key);
        let button = held.then_some(crate::mouse::MouseButton::Left);
        let bytes = crate::mouse::encode_motion(button, c, r, mods, state.encoding);
        if let Some(pane) = self.ws_mut().pane_mut(id) {
            pane.write(&bytes);
            true
        } else {
            false
        }
    }

    /// Wheel notch from the window host (`lines` > 0 scrolls up). Goes to
    /// the pane under the cursor as buttons 64/65 when it tracks the mouse,
    /// else the focused pane's scrollback exactly as before. Returns whether
    /// the wheel was forwarded (false = scrolled back instead).
    pub fn mouse_wheel(&mut self, col: u16, row: u16, lines: i32, mods: Mods) -> bool {
        if lines != 0 && self.config.mouse.enabled && self.mode() == Mode::Normal
            && let Some((id, c, r)) = self.inner_at(col, row) {
                let state = match self.ws().pane(id) {
                    Some(pane) => pane.mouse_state(),
                    None => return false,
                };
                if state.tracking {
                    for _ in 0..lines.unsigned_abs() {
                        let bytes =
                            crate::mouse::encode_wheel(lines > 0, c, r, mods, state.encoding);
                        if let Some(pane) = self.ws_mut().pane_mut(id) {
                            pane.write(&bytes);
                        } else {
                            break;
                        }
                    }
                    return true;
                }
            }
        self.scroll_focused(lines);
        false
    }

    /// Title for the OS window: focused pane of the current workspace.
    pub fn window_title(&self) -> String {
        let ws = self.ws();
        let name = match ws.pane(ws.focused) {
            Some(p) if p.dead => "exited".to_string(),
            Some(p) => p.title.clone(),
            None => "shell".to_string(),
        };
        format!("termrs - {} [{}]", name, ws.name)
    }

    /// Open the config in `$EDITOR`/`$VISUAL` **inside the focused pane**.
    /// The editor runs in the pane's shell like any command; when it saves,
    /// the existing hot reload picks the change up (ctrl+e).
    fn edit_config(&mut self) {
        let Some(path) = self.config.source.clone() else {
            self.status = "edit: no config file loaded (use -c <path>)".into();
            return;
        };
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| if cfg!(windows) { "notepad".into() } else { "vi".into() });
        let cmd = edit_invocation(&editor, &path);
        log::info!("editing config in pane: {cmd}");
        let ws = self.ws_mut();
        if let Some(pane) = ws.pane_mut(ws.focused) {
            pane.write(cmd.as_bytes());
            pane.write(b"\r");
        }
        self.status = format!("editing config in pane: {cmd}");
    }

    /// Reload the config file. On error keep the current config and show the
    /// parse error (with line/column) in the status line.
    pub fn reload_config(&mut self) {
        let Some(path) = self.config.source.clone() else {
            return;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("config reload failed: {e}");
                return;
            }
        };
        match Config::parse_with_source(&text, Some(path.clone())) {
            Ok(new_cfg) => {
                self.apply_config(new_cfg);
                self.status = format!("config reloaded from {}", path.display());
                log::info!("config reloaded from {}", path.display());
            }
            Err(e) => {
                // Show the first line of the TOML error (it carries the
                // line/column) and keep the working config.
                let first = e.lines().next().unwrap_or("invalid config").to_string();
                self.status = format!("config error (kept old): {first}");
                log::warn!("config reload failed: {e}");
            }
        }
    }

    /// Apply a freshly loaded config, refreshing derived state.
    fn apply_config(&mut self, new_cfg: Config) {
        let old_size = self.config.general.font_size.max(6);
        let old_font = self.config.general.font.clone();
        self.config = new_cfg;
        self.theme = self.config.theme.resolve();
        let size = self.config.general.font_size.max(6);
        if size != old_size {
            self.font_size = size;
            self.zoom_request = Some(size);
        }
        // A changed `[general] font` (hand-edited or picked) rebuilds too.
        if self.config.general.font != old_font {
            self.font_request = Some(self.config.general.font.clone());
        }
        // `[window]` (backdrop, decorations, theme for rebuilds) is picked
        // up by the window host, which mirrors this config every frame.
        self.config_mtime = self
            .config
            .source
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        // Reopen the command database if its path changed.
        let db_path = self.config.commands_db_path();
        if db_path != self.db_path {
            self.db_path = db_path;
            self.db = match CommandDb::open(&self.db_path) {
                Ok(d) => Some(d),
                Err(e) => {
                    log::warn!("command db: {e}");
                    None
                }
            };
        }
    }

    /// Hot reload: if the config file changed on disk, reload it.
    /// Polled every ~0.5s (a `stat` per frame would be wasteful).
    pub fn hot_reload_check(&mut self) {
        self.reload_tick = self.reload_tick.wrapping_add(1);
        if !self.reload_tick.is_multiple_of(30) {
            return;
        }
        let Some(path) = self.config.source.clone() else {
            return;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_some() && mtime != self.config_mtime {
            self.config_mtime = mtime;
            self.reload_config();
        }
    }

    /// Toggle the scrollback viewer for the focused pane (modal).
    fn toggle_scrollback(&mut self) {
        if self.scrollback.is_some() {
            self.scrollback = None;
            return;
        }
        let ws = self.ws();
        if let Some(pane) = ws.pane(ws.focused) {
            self.scrollback = Some(ScrollbackView::open(pane));
        }
    }

    /// Scrollback viewer keys (modal: consumes everything while open).
    fn handle_scrollback_key(&mut self, ev: KeyPress) {
        match ev.key {
            Key::Esc => {
                self.scrollback = None;
                return;
            }
            Key::Char('r') if ev.mods.is_empty() => {
                let fid = self.ws().focused;
                let snap = self.ws().pane(fid).map(crate::scrollback::snapshot);
                if let (Some(v), Some((lines, width))) = (&mut self.scrollback, snap) {
                    v.set_snapshot(lines, width);
                }
                return;
            }
            _ => {}
        }
        if keys::matches_any(&ev, &self.config.keys.scroll_view) {
            self.scrollback = None;
            return;
        }
        if let Some(v) = &mut self.scrollback {
            v.input(ev.key);
        }
    }

    /// Copy the shell's last command output to the clipboard (ctrl+y).
    fn yank_last_output(&mut self) {
        let fid = self.ws().focused;
        let text = self
            .ws()
            .pane(fid)
            .map(|p| p.last_output())
            .unwrap_or_default();
        if text.is_empty() {
            self.status = "yank: no output yet".into();
            return;
        }
        self.report_copy(&text);
    }

    /// Enter/leave visual selection over the focused pane.
    fn toggle_select(&mut self) {
        if self.select.is_some() {
            self.select = None;
            self.status.clear();
            return;
        }
        let fid = self.ws().focused;
        if let Some(pane) = self.ws().pane(fid) {
            self.select = Some(SelectMode::enter(pane));
            self.status = "select: hjkl move (k/j at edge scrolls history), w/b/e words, x/X line, % all, f/t find, mi/ma/mm obj, g goto, v anchor, ; collapse, alt+; flip, y yank, esc exit".into();
        }
    }

    /// Scroll the focused pane under an active selection (`delta` > 0 moves
    /// up into history) and refresh the snapshot so the cursor stays on its
    /// edge row showing new content. Returns false when there is no
    /// selection, no pane, or no more history in that direction.
    fn select_scroll(&mut self, delta: i32) -> bool {
        if self.select.is_none() {
            return false;
        }
        let fid = self.ws().focused;
        let before = match self.ws().pane(fid) {
            Some(p) => p.screen().scrollback(),
            None => return false,
        };
        {
            let ws = self.ws_mut();
            match ws.pane_mut(fid) {
                Some(p) => p.scroll(delta),
                None => return false,
            }
        }
        let (after, lines) = match self.ws().pane(fid) {
            Some(p) => (p.screen().scrollback(), p.grid_lines()),
            None => return false,
        };
        if after == before {
            return false;
        }
        // Actual step may be smaller than requested at the history limits;
        // shift the anchor by what really moved.
        let moved = after as i32 - before as i32;
        if let Some(sel) = self.select.as_mut() {
            sel.refresh_after_scroll(lines, moved);
        }
        true
    }

    /// Keys while visual selection is active (modal).
    fn handle_select_key(&mut self, ev: KeyPress) {
        // Leave on Esc or the select-mode key again.
        if matches!(ev.key, Key::Esc) || keys::matches_any(&ev, &self.config.keys.select_mode) {
            self.select = None;
            self.status.clear();
            return;
        }
        let Some(sel) = self.select.as_mut() else {
            return;
        };

        // Helix-style multi-key prefixes: `m` (match) and `g` (goto).
        if let Some(p) = sel.pending.take() {
            if let Key::Char(ch) = ev.key {
                match (p, ch) {
                    ('m', 'i') => sel.pending = Some('i'),
                    ('m', 'a') => sel.pending = Some('a'),
                    ('m', 'm') => sel.match_bracket(),
                    ('i', o) => sel.select_object(o, true),
                    ('a', o) => sel.select_object(o, false),
                    ('f', c) => sel.find_char(c, true),
                    ('F', c) => sel.find_char(c, false),
                    ('t', c) => sel.find_till(c, true),
                    ('T', c) => sel.find_till(c, false),
                    ('g', c) => {
                        sel.goto(c);
                    }
                    _ => {}
                }
            }
            return;
        }

        // `Alt-;` flips the selection ends (helix); plain `;` collapses.
        if matches!(ev.key, Key::Char(';')) && ev.mods.contains(Mods::ALT) {
            sel.swap_ends();
            return;
        }
        // Release the borrow: vertical motions may scroll the pane, which
        // needs `&mut self` for the snapshot refresh.
        let _ = sel;

        match ev.key {
            Key::Char('h') | Key::Left => {
                if let Some(sel) = self.select.as_mut() {
                    sel.move_by(-1, 0);
                }
            }
            Key::Char('l') | Key::Right => {
                if let Some(sel) = self.select.as_mut() {
                    sel.move_by(1, 0);
                }
            }
            Key::Char('k') | Key::Up => {
                // At the top edge, scroll back into history instead of
                // clamping: the snapshot refreshes and the cursor stays on
                // row 0 over the newly revealed line.
                let at_top = self
                    .select
                    .as_ref()
                    .is_some_and(|s| s.at_top());
                if at_top && self.select_scroll(1) {
                    return;
                }
                if let Some(sel) = self.select.as_mut() {
                    sel.move_by(0, -1);
                }
            }
            Key::Char('j') | Key::Down => {
                // Symmetric: at the bottom edge, scroll forward toward the
                // live screen.
                let at_bottom = self
                    .select
                    .as_ref()
                    .is_some_and(|s| s.at_bottom());
                if at_bottom && self.select_scroll(-1) {
                    return;
                }
                if let Some(sel) = self.select.as_mut() {
                    sel.move_by(0, 1);
                }
            }
            // Helix word motions (`W`/`B`/`E` = WORDs).
            Key::Char('w') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_forward(false);
                }
            }
            Key::Char('W') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_forward(true);
                }
            }
            Key::Char('b') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_backward(false);
                }
            }
            Key::Char('B') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_backward(true);
                }
            }
            Key::Char('e') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_end(false);
                }
            }
            Key::Char('E') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.word_end(true);
                }
            }
            // Helix line selections.
            Key::Char('x') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.extend_line();
                }
            }
            Key::Char('X') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.extend_line_bounds();
                }
            }
            Key::Char('%') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.select_all();
                }
            }
            // Home/End/0/$ go to the start/end of the cursor's own line.
            Key::Home | Key::Char('0') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.cur = (sel.cur.0, 0);
                }
            }
            Key::End | Key::Char('$') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.cur = (sel.cur.0, sel.cols.saturating_sub(1));
                }
            }
            Key::Char('m') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('m');
                }
            }
            Key::Char('g') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('g');
                }
            }
            Key::Char('f') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('f');
                }
            }
            Key::Char('F') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('F');
                }
            }
            Key::Char('t') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('t');
                }
            }
            Key::Char('T') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.pending = Some('T');
                }
            }
            // Collapse the selection onto the cursor (helix `;`).
            Key::Char(';') => {
                if let Some(sel) = self.select.as_mut() {
                    sel.collapse();
                }
            }
            Key::Char('v') | Key::Char(' ') | Key::Enter => {
                if let Some(sel) = self.select.as_mut() {
                    sel.toggle_anchor();
                }
            }
            Key::Char('y') => {
                let text = self
                    .select
                    .as_ref()
                    .map(|s| s.selected_text())
                    .unwrap_or_default();
                self.select = None;
                self.report_copy(&text);
            }
            _ => {}
        }
    }

    /// Copy `text`, updating the status line with the outcome.
    fn report_copy(&mut self, text: &str) {
        if text.is_empty() {
            self.status = "yank: nothing to copy".into();
            return;
        }
        let lines = text.lines().count();
        match crate::clipboard::copy(text) {
            Ok(()) => self.status = format!("yanked {lines} line(s)"),
            Err(e) => self.status = format!("yank failed: {e}"),
        }
    }

    /// Switch to workspace `i` (acknowledge its activity, flash its pane).
    fn switch_to(&mut self, i: usize) {
        if i >= self.workspaces.len() {
            return;
        }
        self.current = i;
        let id = self.ws().focused;
        self.ws_mut().mark_seen();
        self.queue_fx(id, FxKind::Focus);
    }

    /// Switch relatively (wraps around).
    fn switch_relative(&mut self, dir: i32) {
        let n = self.workspaces.len();
        if n <= 1 {
            return;
        }
        let next = (self.current as i32 + dir).rem_euclid(n as i32) as usize;
        self.switch_to(next);
    }

    /// Move workspace `from` to position `to`, keeping `current` pointed at
    /// the same workspace it pointed at before. Out-of-range or no-op moves
    /// are ignored. Playing close transitions finish first so no stale
    /// workspace indices survive, and the new order is saved to disk.
    fn move_workspace(&mut self, from: usize, to: usize) {
        let n = self.workspaces.len();
        if from == to || from >= n || to >= n {
            return;
        }
        self.complete_closing();
        let ws = self.workspaces.remove(from);
        self.workspaces.insert(to, ws);
        self.current = if self.current == from {
            to
        } else {
            let mut c = self.current;
            if from < c {
                c -= 1;
            }
            if to <= c {
                c += 1;
            }
            c
        };
        log::info!("moved workspace {from} -> {to}");
    }

    /// Create workspace `ws-N` and switch to it.
    fn new_workspace(&mut self) {
        // Reuse the smallest free `ws-N` number instead of counting up.
        let mut n = 1usize;
        while self.workspaces.iter().any(|w| w.name == format!("ws-{n}")) {
            n += 1;
        }
        let shell = self.config.general.shell.clone();
        let sb = self.config.general.scrollback;
        let rt = self.rt.clone();
        let wake = self.wake.clone();
        match Workspace::new(format!("ws-{n}"), &shell, sb, &rt, &wake) {
            Ok(ws) => {
                self.workspaces.push(ws);
                self.current = self.workspaces.len() - 1;
                self.ws_mut().mark_seen();
                self.status = format!("workspace ws-{n}");
                self.queue_fx(0, FxKind::Fresh);
                log::info!("new workspace ws-{n}");
            }
            Err(e) => self.status = format!("workspace failed: {e}"),
        }
    }

    /// Close current workspace; quits when it was the last one.
    fn close_current_workspace(&mut self) {
        self.complete_closing();
        if self.workspaces.len() <= 1 {
            log::info!("quit: closed the last workspace/pane");
            self.should_quit = true;
            return;
        }
        let cur = self.current;
        self.drop_workspace(cur);
    }
}

/// Translate our backend-agnostic [`KeyPress`] into a `ratatui-textarea`
/// [`Input`], so prompts keep their Emacs-style editing without crossterm.
fn textarea_input(ev: &KeyPress) -> Input {
    Input {
        key: match ev.key {
            Key::Char(c) => TaKey::Char(c),
            Key::Space => TaKey::Char(' '),
            Key::Enter => TaKey::Enter,
            Key::Esc => TaKey::Esc,
            Key::Tab | Key::BackTab => TaKey::Tab,
            Key::Backspace => TaKey::Backspace,
            Key::Delete => TaKey::Delete,
            Key::Up => TaKey::Up,
            Key::Down => TaKey::Down,
            Key::Left => TaKey::Left,
            Key::Right => TaKey::Right,
            Key::Home => TaKey::Home,
            Key::End => TaKey::End,
            Key::PageUp => TaKey::PageUp,
            Key::PageDown => TaKey::PageDown,
            Key::F(n) => TaKey::F(n),
            Key::Insert => TaKey::Null,
        },
        ctrl: ev.mods.contains(Mods::CONTROL),
        alt: ev.mods.contains(Mods::ALT),
        shift: ev.mods.contains(Mods::SHIFT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn key(key: Key, mods: Mods) -> KeyPress {
        KeyPress {
            key,
            mods,
            text: None,
            kind: KeyKind::Press,
        }
    }

    /// Closing a pane keeps it in the tree with a Close transition queued,
    /// then removes it once the effect deadline passes.
    #[test]
    fn close_pane_plays_transition_then_removes() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("close");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        assert_eq!(app.ws().leaf_ids().len(), 1);

        // ctrl+s splits (default bindings), ctrl+w closes.
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);

        app.handle_key(key(Key::Char('w'), Mods::CONTROL));
        // Still present while the close transition plays.
        assert_eq!(app.ws().leaf_ids().len(), 2);
        assert!(!app.closing.is_empty());
        let pending = app.drain_fx();
        assert!(pending.iter().any(|p| p.kind == FxKind::Close));

        // After the deadline the leaf is gone; the layout is only persisted
        // on demand, never automatically.
        std::thread::sleep(std::time::Duration::from_millis(500));
        app.poll_panes();
        assert!(app.closing.is_empty());
        assert_eq!(app.ws().leaf_ids().len(), 1);
        assert!(!layout.is_file(), "layout must not be auto-saved");
        app.run_command(Command::SaveLayout);
        assert!(layout.is_file(), "Save layout command persists the layout");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// With fx disabled the pane is removed immediately.
    #[test]
    fn close_pane_instant_when_fx_disabled() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("close-off");
        let mut cfg = Config::default();
        cfg.fx.enabled = false;
        let mut app = App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);
        app.handle_key(key(Key::Char('w'), Mods::CONTROL));
        assert!(app.closing.is_empty());
        assert_eq!(app.ws().leaf_ids().len(), 1);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+shift+z maximizes the focused pane; focus keys restore the layout
    /// AND move in one press (directional focus uses tiled geometry);
    /// splitting clears zoom.
    #[test]
    fn zoom_pane_toggle_and_focus_moves() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("zoom-pane");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        // ctrl+shift+z is bound to ZoomPane by default.
        assert!(matches!(
            app.lookup_command(&key(Key::Char('Z'), Mods::CONTROL | Mods::SHIFT)),
            Some(Command::ZoomPane)
        ));
        // Single pane: zoom is a no-op.
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), None);

        // Split (stacked: old on top, new focused below), then maximize the
        // focused pane via the real keypress.
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        let ids = app.ws().leaf_ids();
        assert_eq!(ids.len(), 2);
        let (top, bottom) = (ids[0], ids[1]);
        assert_eq!(app.ws().focused, bottom);
        app.handle_key(key(Key::Char('Z'), Mods::CONTROL | Mods::SHIFT));
        assert_eq!(app.ws().zoomed(), Some(bottom));
        assert!(app.status.contains("maximized"));

        // Seed tiled geometry the way a drawn frame would (tests never draw).
        app.ws_mut().set_tiled_rects(&[
            (top, ratatui::layout::Rect::new(0, 0, 80, 20)),
            (bottom, ratatui::layout::Rect::new(0, 20, 80, 20)),
        ]);
        // A focus key restores AND moves in one press.
        app.run_command(Command::FocusUp);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, top);
        assert!(app.status.contains("restored"));
        // Cycling also restores and moves (wraps to the other pane).
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), Some(top));
        app.run_command(Command::FocusNext);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, bottom);
        // Nothing that way: still restores, focus stays put.
        app.run_command(Command::ZoomPane);
        app.run_command(Command::FocusDown);
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().focused, bottom);

        // Toggling twice restores as well.
        app.run_command(Command::ZoomPane);
        assert!(app.ws().zoomed().is_some());
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), None);

        // Splitting while zoomed restores first so the new pane is visible.
        app.run_command(Command::ZoomPane);
        assert!(app.ws().zoomed().is_some());
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().zoomed(), None);
        assert_eq!(app.ws().leaf_ids().len(), 3);

        // Closing the zoomed pane clears the zoom.
        let target = app.ws().focused;
        app.run_command(Command::ZoomPane);
        assert_eq!(app.ws().zoomed(), Some(target));
        app.ws_mut().remove_leaf(target);
        assert_eq!(app.ws().zoomed(), None);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Splitting opens the new shell in the focused pane's directory; a
    /// deleted directory falls back to the process directory.
    #[test]
    fn split_inherits_focused_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("split-cwd");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let dir = std::env::temp_dir().join(format!(
            "termrs-split-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Pretend the focused shell reported this directory.
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .set_cwd_for_test(dir.clone());
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 2);
        let nid = app.ws().focused;
        assert_ne!(nid, fid);
        assert_eq!(
            app.ws().pane(nid).expect("new pane").cwd(),
            dir.as_path(),
            "split shell starts in the focused directory"
        );

        // Deleted directory: split still works, from the process directory.
        let gone = dir.join("gone-missing");
        app.ws_mut()
            .pane_mut(nid)
            .expect("new pane")
            .set_cwd_for_test(gone);
        app.handle_key(key(Key::Char('s'), Mods::CONTROL));
        assert_eq!(app.ws().leaf_ids().len(), 3);
        let nid2 = app.ws().focused;
        assert_eq!(
            app.ws().pane(nid2).expect("third pane").cwd(),
            std::env::current_dir().unwrap().as_path()
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Helix-style text objects and g-motions.
    #[test]
    fn select_text_objects_and_goto() {
        let line = "let foo = bar(1, 2);".to_string();
        // miw: inside word under cursor.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 5));
        sel.select_object('w', true);
        assert_eq!(sel.selected_text(), "foo");
        // maw includes trailing spaces.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 5));
        sel.select_object('w', false);
        assert_eq!(sel.selected_text(), "foo");
        // mi( : inside parentheses.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 15));
        sel.select_object('(', true);
        assert_eq!(sel.selected_text(), "1, 2");
        // ma( : around parentheses.
        let mut sel = SelectMode::from_lines(vec![line.clone()], (0, 15));
        sel.select_object('(', false);
        assert_eq!(sel.selected_text(), "(1, 2)");
        // gl / gh / gg / ge.
        let mut sel = SelectMode::from_lines(vec![line.clone(); 3], (1, 4));
        assert!(sel.goto('l'));
        assert_eq!(sel.cursor(), (1, 19));
        assert!(sel.goto('h'));
        assert_eq!(sel.cursor(), (1, 0));
        assert!(sel.goto('g'));
        assert_eq!(sel.cursor(), (0, 0));
        assert!(sel.goto('e'));
        assert_eq!(sel.cursor(), (2, 0));
        assert!(!sel.goto('z'));
    }

    /// `;` swaps the selection ends; `gl` hits the last char; `f`/`F` find.
    #[test]
    fn select_swap_goto_and_find() {
        let line = "a,b,c   ".to_string();
        let mut sel = SelectMode::from_lines(vec![line], (0, 0));
        sel.anchor = Some((0, 2));
        sel.cur = (0, 4);
        sel.swap_ends();
        assert_eq!(sel.anchor, Some((0, 4)));
        assert_eq!(sel.cur, (0, 2));
        // gl lands on the last non-blank character, not the padded end.
        sel.goto('l');
        assert_eq!(sel.cur, (0, 4));
        // f forward to the next ','.
        sel.cur = (0, 0);
        sel.find_char(',', true);
        assert_eq!(sel.cur, (0, 1));
        // F backward from the end.
        sel.cur = (0, 4);
        sel.find_char(',', false);
        assert_eq!(sel.cur, (0, 3));
        // Missing target leaves the cursor put.
        sel.cur = (0, 1);
        sel.find_char('z', true);
        assert_eq!(sel.cur, (0, 1));
    }

    /// Helix word motions: `w`/`b`/`e` on words, `W`/`B`/`E` on WORDs,
    /// crossing line boundaries.
    #[test]
    fn select_helix_word_motions() {
        let lines = vec!["foo bar  ".to_string(), "  baz".to_string()];
        let mut sel = SelectMode::from_lines(lines.clone(), (0, 0));
        // w: next word start.
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (0, 4));
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (1, 2));
        // At the last word: stay.
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (1, 4));
        // b: to the start of the same word, then previous (crosses lines).
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (1, 2));
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 4));
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 0));
        // At the first cell: stay.
        sel.word_backward(false);
        assert_eq!(sel.cursor(), (0, 0));
        // e: next word end.
        sel.word_end(false);
        assert_eq!(sel.cursor(), (0, 2));
        sel.word_end(false);
        assert_eq!(sel.cursor(), (0, 6));
        // WORD motions skip punctuation runs: "foo,bar baz".
        let mut sel = SelectMode::from_lines(vec!["foo,bar baz".to_string()], (0, 0));
        sel.word_forward(true);
        assert_eq!(sel.cursor(), (0, 8));
        sel.cur = (0, 0);
        sel.word_end(true);
        assert_eq!(sel.cursor(), (0, 6));
        sel.word_backward(true);
        assert_eq!(sel.cursor(), (0, 0));
        // Lowercase stops at punctuation.
        let mut sel = SelectMode::from_lines(vec!["foo,bar baz".to_string()], (0, 0));
        sel.word_forward(false);
        assert_eq!(sel.cursor(), (0, 4));
    }

    /// Helix line selections: `x` (extend, then next line), `X` (bounds),
    /// `%` (whole grid), `;` (collapse onto cursor).
    #[test]
    fn select_helix_line_objects() {
        let lines = vec!["ab  ".to_string(), "cdef".to_string()];
        // x selects the cursor's line up to the last non-blank.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 1));
        sel.extend_line();
        assert_eq!(sel.anchor, Some((1, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        assert_eq!(sel.selected_text(), "cdef");
        // x again on a full line extends through the next line's end.
        let mut sel = SelectMode::from_lines(lines.clone(), (0, 0));
        sel.extend_line();
        assert_eq!(sel.selected_text(), "ab");
        sel.cur = (0, 1);
        sel.extend_line();
        assert_eq!(sel.selected_text(), "ab\ncdef");
        // X extends to the line bounds.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 2));
        sel.extend_line_bounds();
        assert_eq!(sel.anchor, Some((1, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        // % selects the whole grid.
        let mut sel = SelectMode::from_lines(lines.clone(), (1, 1));
        sel.select_all();
        assert_eq!(sel.anchor, Some((0, 0)));
        assert_eq!(sel.cursor(), (1, 3));
        // ; collapses onto the cursor (zero-width, still a range).
        sel.collapse();
        assert_eq!(sel.anchor, Some((1, 3)));
        assert_eq!(sel.range(), Some((1, 3, 1, 3)));
    }

    /// Helix `t`/`T` stop one cell short; `mm` jumps to the matching bracket.
    #[test]
    fn select_helix_till_and_match() {
        let mut sel = SelectMode::from_lines(vec!["abca".to_string()], (0, 0));
        sel.find_till('c', true);
        assert_eq!(sel.cursor(), (0, 1));
        // Adjacent target: stay put. No match ahead: stay put.
        sel.cur = (0, 0);
        sel.find_till('b', true);
        assert_eq!(sel.cursor(), (0, 0));
        sel.find_till('z', true);
        assert_eq!(sel.cursor(), (0, 0));
        sel.cur = (0, 3);
        sel.find_till('a', false);
        assert_eq!(sel.cursor(), (0, 1));
        // mm jumps between matching brackets, nesting-aware.
        let mut sel = SelectMode::from_lines(vec!["((a))".to_string()], (0, 0));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 4));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 0));
        let mut sel = SelectMode::from_lines(vec!["[x] (y)".to_string()], (0, 4));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 6));
        // Not on a bracket: stay put.
        let mut sel = SelectMode::from_lines(vec!["abc".to_string()], (0, 1));
        sel.match_bracket();
        assert_eq!(sel.cursor(), (0, 1));
    }

    /// Edit command quoting (paths with spaces work).
    #[test]
    fn edit_invocation_quotes_path() {
        let p = std::path::Path::new("C:\\a b\\config.toml");
        let cmd = edit_invocation("notepad", p);
        assert!(cmd.starts_with("notepad "), "cmd: {cmd}");
        assert!(cmd.contains("config.toml"), "cmd: {cmd}");
        // The path is quoted for the platform shell.
        assert!(
            cmd.contains("\"C:\\a b\\config.toml\"") || cmd.contains("'C:\\a b\\config.toml'"),
            "cmd: {cmd}"
        );
    }

    /// `cursor_cell` maps the focused pane's cursor into the inner (border-less)
    /// pane area, for the GPU bar/underline cursor.
    #[test]
    fn cursor_cell_uses_inner_pane_area() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cursorcell");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 20, 10))]);
        let (inner, row, col) = app.cursor_cell().expect("cursor visible");
        assert_eq!(inner, ratatui::layout::Rect::new(1, 1, 18, 8));
        assert_eq!((row, col), (0, 0));
        // Default theme asks for a bar cursor.
        assert_eq!(
            app.theme_colors().cursor_shape,
            crate::config::CursorShape::Bar
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_file(&layout);
    }

    /// While selecting, the GPU cursor hides: the selection overlay owns the
    /// cursor, so only the highlighted end shows.
    #[test]
    fn cursor_cell_hidden_while_selecting() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cursorselect");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 20, 10))]);
        assert!(app.cursor_cell().is_some(), "cursor visible normally");
        app.toggle_select();
        assert!(app.select.is_some(), "select mode entered");
        assert!(app.cursor_cell().is_none(), "GPU cursor hidden while selecting");
        app.toggle_select();
        assert!(app.cursor_cell().is_some(), "cursor back after leaving");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_file(&layout);
    }

    /// Empty input completes from the shell's cwd; images rank first.
    #[test]
    fn completion_lists_directory_from_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("complete");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let dir = std::env::temp_dir().join(format!("termrs-cwd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("zed.png"), b"x").unwrap();
        std::fs::write(dir.join("alpha.txt"), b"x").unwrap();
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .set_cwd_for_test(dir.clone());

        // Empty input -> completes an entry from the directory (image first).
        let ghost = app.complete_path("").expect("a completion");
        assert_eq!(ghost, "zed.png", "completes from empty input: {ghost:?}");
        // Typed prefix narrows it.
        assert_eq!(app.complete_path("al").expect("match"), "pha.txt");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `exit` in the only pane quits the app instead of lingering.
    #[test]
    fn exit_in_last_pane_quits() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("exit");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .pane_mut(fid)
            .expect("focused pane")
            .write(b"exit\r");
        let mut quit = false;
        for _ in 0..200 {
            app.poll_panes();
            if app.should_quit {
                quit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(quit, "app quits when its last shell exits");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// New workspaces reuse the smallest free `ws-N` number.
    #[test]
    fn workspace_index_reused_after_close() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("idx");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.new_workspace();
        app.new_workspace();
        assert_eq!(app.workspaces[1].name, "ws-1");
        assert_eq!(app.workspaces[2].name, "ws-2");
        // Close ws-1 (switch to it first), then create: ws-1 comes back.
        app.switch_to(1);
        app.close_current_workspace();
        assert_eq!(app.workspaces.len(), 2);
        app.new_workspace();
        assert_eq!(app.workspaces[2].name, "ws-1");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        // Detach without waiting for stuck PTY readers (see run_inner).
        rt.shutdown_background();
    }

    /// Zoom keys queue a font-size request for the window host.
    #[test]
    fn zoom_queues_font_size_request() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("zoom");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        assert_eq!(app.font_size, 16);
        app.handle_key(key(Key::Char('+'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), Some(17));
        assert_eq!(app.font_size, 17);
        app.handle_key(key(Key::Char('-'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), Some(16));
        app.handle_key(key(Key::Char('0'), Mods::CONTROL));
        assert_eq!(app.take_zoom_request(), None);
        assert!(app.status.contains("16px"));
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The font picker filters fuzzily and always offers the system default
    /// first (pure logic, no panes needed).
    #[test]
    fn font_picker_filters_and_lists_default_first() {
        let entries = vec![
            FontEntry {
                name: "Zeta".to_string(),
                path: PathBuf::from("/fonts/Zeta.ttf"),
                mono: Some(true),
            },
            FontEntry {
                name: "alpha".to_string(),
                path: PathBuf::from("/fonts/alpha.ttf"),
                mono: Some(false),
            },
        ];
        let mut p = FontPicker::new(entries, "/fonts/Zeta.ttf", 18);
        assert_eq!(p.results.len(), 3);
        assert!(p.results[0].item.path.as_os_str().is_empty());
        assert_eq!(p.active, "/fonts/Zeta.ttf");
        assert_eq!(p.size, 18);
        assert_eq!(p.focus, PickerFocus::List);
        // The active font is preselected so its preview shows immediately.
        assert_eq!(p.results[p.selected].item.name, "Zeta");
        p.query = "alp".to_string();
        p.refilter();
        assert_eq!(p.results.len(), 1);
        assert_eq!(p.results[0].item.name, "alpha");
        // The probe flag survives filtering (drives the row warning tag).
        assert_eq!(p.results[0].mono, Some(false));
        p.query = "zzz-no-match".to_string();
        p.refilter();
        assert!(p.results.is_empty());
        p.query.clear();
        p.refilter();
        assert_eq!(p.results.len(), 3);
    }

    /// Visible-row probing fills unknown flags and the cache survives
    /// refilters; unparsable files stay untagged without panicking.
    #[test]
    fn font_probe_visible_fills_and_caches() {
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontprobe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font").unwrap();
        let mut p = FontPicker::new(
            vec![FontEntry {
                name: "junk".to_string(),
                path: junk,
                mono: None,
            }],
            "",
            16,
        );
        p.probe_visible(0, 10);
        assert!(p.results[1].mono.is_none(), "unparsable stays untagged");

        // A real installed font (when one exists) probes to Some and the
        // flag survives refiltering.
        if let Some(real) = crate::font::scan_fonts().into_iter().next() {
            let mut p = FontPicker::new(vec![real], "", 16);
            p.probe_visible(0, 10);
            assert!(p.results[1].mono.is_some());
            p.query = "zzz-no-match-ever".to_string();
            p.refilter();
            p.query.clear();
            p.refilter();
            assert!(p.results[1].mono.is_some(), "cached across refilters");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Picking a font updates `[general] font` in memory and on disk and
    /// queues a live rebuild for the window host.
    #[test]
    fn font_pick_updates_config_and_queues_rebuild() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontpick-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "# comment\n[general]\nfont_size = 16\n").unwrap();
        let fake_font = dir.join("Fake.ttf");
        std::fs::write(&fake_font, b"not a real font, only readable").unwrap();
        let layout = dir.join("layout.toml");

        let cfg = Config::load(Some(cfg_path.as_path()));
        let mut app = App::new(cfg, Some(layout), rt.handle(), &wake, None, None, false).expect("app boots");
        let entry = FontEntry {
            name: "Fake".to_string(),
            path: fake_font.clone(),
            mono: None,
        };
        app.font_picker = Some(FontPicker::new(vec![entry], "", 16));
        // Move past the "system default" first row onto the fake font.
        app.handle_key(key(Key::Down, Mods::empty()));
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.font_picker.is_none(), "picker closes on pick");
        let want = fake_font.to_string_lossy().into_owned();
        assert_eq!(app.config.general.font, want);
        assert_eq!(app.config.general.font_size, 16);
        assert_eq!(app.take_font_request(), Some(want.clone()));
        assert!(app.status.contains("Fake"), "status: {}", app.status);
        let on_disk = std::fs::read_to_string(&cfg_path).unwrap();
        assert!(on_disk.contains("# comment"), "comments survive: {on_disk}");
        let reloaded: Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(reloaded.general.font, want);
        assert_eq!(reloaded.general.font_size, 16);

        // The default entry clears the field back to auto-detect.
        app.font_picker = Some(FontPicker::new(Vec::new(), &want, 16));
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert_eq!(app.config.general.font, "");
        assert_eq!(app.take_font_request(), Some(String::new()));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preview worker answers off-thread: the latest request wins, stale
    /// results are dropped, repeats are not re-sent, and closing the picker
    /// drops the worker channels.
    #[test]
    fn font_preview_worker_delivers_latest_only() {
        let Some(real) = crate::font::scan_fonts().into_iter().next() else {
            return; // No installed fonts to render; nothing to prove.
        };
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("fontpreview");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let entry = FontEntry {
            name: real.name.clone(),
            path: real.path.clone(),
            mono: None,
        };
        app.font_picker = Some(FontPicker::new(vec![entry], "", 24));
        // Past the "system default" row onto the real font.
        app.font_picker.as_mut().expect("picker").selected = 1;
        app.start_preview_worker();
        assert!(!app.font_preview_loading());

        // Two rapid requests (simulating fast up/down): only the second
        // generation may land on screen.
        app.request_font_preview();
        let gen1 = app.font_preview_gen;
        assert!(app.font_preview_loading());
        app.font_picker.as_mut().expect("picker").nudge_size(8);
        app.request_font_preview();
        assert_eq!(app.font_preview_gen, gen1 + 1);
        // Same (path, size) twice: no resend, no generation bump.
        app.request_font_preview();
        assert_eq!(app.font_preview_gen, gen1 + 1);

        // Poll until the latest render lands (stale ones are discarded).
        let want_size = app.font_picker.as_ref().expect("picker").size;
        let mut landed = false;
        for _ in 0..400 {
            app.poll_font_preview();
            if !app.font_preview_loading()
                && let Some(((path, size), _, _)) = app.font_preview_meta()
                    && path == real.path.to_string_lossy() && size == want_size {
                        landed = true;
                        break;
                    }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(landed, "latest preview never landed");
        assert!(!app.font_preview_loading());

        // Closing the picker drops the worker channels.
        app.cancel_font_picker();
        assert!(app.font_preview_tx.is_none());
        assert!(app.font_preview_rx.is_none());
        assert!(!app.font_preview_loading());

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Tab focuses the size field; arrows/j/k and digits adjust the size
    /// live (queued as a zoom rebuild); Enter commits it to the config.
    #[test]
    fn font_picker_size_field_adjusts_and_commits() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = std::env::temp_dir().join(format!(
            "termrs-fontsize-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[general]\nfont_size = 16\n").unwrap();
        let layout = dir.join("layout.toml");

        let cfg = Config::load(Some(cfg_path.as_path()));
        let mut app = App::new(cfg, Some(layout), rt.handle(), &wake, None, None, false).expect("app boots");
        app.open_font_picker();
        assert_eq!(app.config.general.font_size, 16);

        // Tab -> size field.
        app.handle_key(key(Key::Tab, Mods::empty()));
        assert_eq!(
            app.font_picker.as_ref().unwrap().focus,
            PickerFocus::Size
        );
        // Up / j step the size; each queues a live zoom rebuild.
        app.handle_key(key(Key::Up, Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 17);
        assert_eq!(app.take_zoom_request(), Some(17));
        app.handle_key(key(Key::Char('k'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 18);
        assert_eq!(app.take_zoom_request(), Some(18));
        app.handle_key(key(Key::Down, Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 17);
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 16);
        assert_eq!(app.take_zoom_request(), Some(16));
        // Typing digits sets an exact size.
        app.handle_key(key(Key::Char('2'), Mods::empty()));
        app.handle_key(key(Key::Char('4'), Mods::empty()));
        assert_eq!(app.font_picker.as_ref().unwrap().size, 24);
        assert_eq!(app.take_zoom_request(), Some(24));

        // Enter on the system-default row commits size (font stays auto).
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.font_picker.is_none());
        assert_eq!(app.config.general.font, "");
        assert_eq!(app.config.general.font_size, 24);
        let reloaded: Config =
            toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(reloaded.general.font_size, 24);

        // Esc restores the entry state (font + size) without writing.
        let mut app = app;
        app.open_font_picker();
        app.handle_key(key(Key::Tab, Mods::empty()));
        app.handle_key(key(Key::Up, Mods::empty()));
        assert_eq!(app.font_size, 25);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.font_picker.is_none());
        assert_eq!(app.font_size, 24, "size restored on cancel");
        assert_eq!(app.take_zoom_request(), Some(24));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A macro binding types its text (plus Enter) into the focused pane.
    #[test]
    fn macro_types_into_focused_pane() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("macro");
        let mut cfg = Config::default();
        cfg.macros.push(crate::config::Macro {
            keys: vec!["ctrl+m".to_string()],
            send: "macro_probe_xyz".to_string(),
            enter: false,
        });
        let mut app = App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");
        assert!(app.handle_key(key(Key::Char('m'), Mods::CONTROL)));
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let fid = app.ws().focused;
            if let Some(p) = app.ws().pane(fid)
                && p.screen().contents().contains("macro_probe_xyz") {
                    seen = true;
                    break;
                }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "macro text reached the pane");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// POST /command text with doubled characters reaches the pane byte-
    /// intact. Regression test: per-character key events arrived as a
    /// microsecond burst of key-down records with no key-up in between,
    /// and ConPTY coalesced back-to-back identical key-downs (`a--b`
    /// arrived as `a-b`). Both the echoed line and the command output
    /// must contain the marker verbatim.
    #[test]
    fn http_command_preserves_doubled_chars() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("httpcmd");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.handle_http_command("echo probe--xx++yy");
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let fid = app.ws().focused;
            if let Some(p) = app.ws().pane(fid)
                && p.screen().contents().contains("probe--xx++yy") {
                    seen = true;
                    break;
                }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "doubled chars survived /command injection");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Selection text: whole grid without an anchor, sub-rect with one.
    #[test]
    fn select_mode_extracts_text() {
        let lines = vec![
            "hello world ".to_string(),
            "second line ".to_string(),
            "third       ".to_string(),
        ];
        let mut sel = SelectMode::from_lines(lines, (1, 0));
        assert_eq!(sel.selected_text(), "hello world\nsecond line\nthird");
        sel.anchor = Some((0, 0));
        sel.cur = (1, 5);
        assert_eq!(sel.selected_text(), "hello world\nsecond");
    }

    /// Movement and anchor toggling clamp to the grid.
    #[test]
    fn select_mode_moves_and_clamps() {
        let mut sel = SelectMode::from_lines(vec!["abc".to_string(); 3], (1, 1));
        sel.move_by(5, 5);
        assert_eq!(sel.cursor(), (2, 2));
        sel.move_by(-9, -9);
        assert_eq!(sel.cursor(), (0, 0));
        assert!(sel.range().is_none());
        sel.toggle_anchor();
        assert!(sel.range().is_some());
        sel.toggle_anchor();
        assert!(sel.range().is_none());
    }

    /// Snapshot refresh after a scroll carries the anchor with the content
    /// (and clamps a cursor/anchor that scrolled out of view).
    #[test]
    fn select_refresh_after_scroll_shifts_anchor() {
        let mut sel = SelectMode::from_lines(vec!["a".to_string(), "b".to_string()], (0, 0));
        sel.anchor = Some((0, 0));
        // Scrolled one line up into history: old content moved down one row.
        sel.refresh_after_scroll(vec!["H".to_string(), "a".to_string()], 1);
        assert_eq!(sel.cursor(), (0, 0));
        assert_eq!(sel.anchor, Some((1, 0)));
        // Scrolled back down to live: content moved back up.
        sel.refresh_after_scroll(vec!["a".to_string(), "b".to_string()], -1);
        assert_eq!(sel.anchor, Some((0, 0)));
        // Anchor at the edge that scrolls out of view clamps instead of
        // under/overflowing.
        sel.anchor = Some((1, 0));
        sel.refresh_after_scroll(vec!["H".to_string(), "a".to_string()], 1);
        assert_eq!(sel.anchor, Some((1, 0)));
    }

    /// In select mode, `k` on the top row scrolls back into history (and
    /// `j` on the bottom row scrolls forward), keeping the cursor on its
    /// edge row over the newly revealed line.
    #[test]
    fn select_edge_scrolls_pane_history() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("select-scroll");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        // More lines than fit on screen, so scrollback history exists.
        let mut fill = String::new();
        for i in 0..60 {
            fill.push_str(&format!("line{i:02}\r\n"));
        }
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(fill.as_bytes());
        app.toggle_select();
        assert!(app.select.is_some(), "select mode entered");
        // Jump to the top row (gg).
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().0, 0);
        let before = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(before, 0);
        // `k` at the top edge scrolls one line into history.
        app.handle_key(key(Key::Char('k'), Mods::empty()));
        let after = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(after, before + 1, "k at top scrolls back");
        assert!(app.select.is_some(), "still in select mode");
        assert_eq!(
            app.select.as_ref().unwrap().cursor().0,
            0,
            "cursor stays on the top row over new content"
        );
        // Jump to the bottom row (ge), then `j` scrolls back toward live.
        app.handle_key(key(Key::Char('g'), Mods::empty()));
        app.handle_key(key(Key::Char('e'), Mods::empty()));
        let rows = app.select.as_ref().unwrap().cursor().0 + 1;
        assert!(rows > 1);
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        let back = app.ws().pane(fid).expect("pane").screen().scrollback();
        assert_eq!(back, after - 1, "j at bottom scrolls forward");
        assert!(app.select.is_some(), "still in select mode");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+o opens the inbox: menu flag set and the dropdown group active.
    #[test]
    fn ctrl_o_opens_menu_dropdown() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menu");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        assert!(!app.menu_open);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(app.menu_open, "ctrl+o opens the menu");
        assert!(app.menu.is_active(), "dropdown group is highlighted/opened");
        // Navigate and select a workspace entry: menu closes again.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Opening the inbox highlights the current workspace's row (not always
    /// the first row), so Enter keeps you where you are.
    #[test]
    fn inbox_opens_on_current_workspace() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menucur");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2, current
        assert_eq!(app.current, 2);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(app.menu_open);
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(2))
            ),
            "highlight starts on the current workspace"
        );
        // Enter selects it: still on ws-2, menu closes.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(!app.menu_open);
        assert_eq!(app.current, 2);
        assert_eq!(app.ws().name, "ws-2");
        // Same when opening from the middle.
        app.switch_to(0);
        app.handle_key(key(Key::Char('o'), Mods::CONTROL));
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(0))
            ),
            "highlight follows a switch"
        );
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The top menu is populated even before the inbox is opened, so the bar
    /// shows `[Workspaces]` from startup.
    #[test]
    fn menu_is_populated_without_opening() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menubar");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.sync_menu();
        // Render the bar into a buffer and check the group labels appear.
        let area = ratatui::layout::Rect::new(0, 0, 60, 1);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::StatefulWidget::render(
            tui_menu::Menu::new(),
            area,
            &mut buf,
            app.menu_mut(),
        );
        let mut text = String::new();
        for x in 0..area.width {
            text.push_str(buf[(x, 0)].symbol());
        }
        assert!(
            text.contains("Workspaces") && text.contains("Actions") && text.contains("Commands"),
            "bar text: {text:?}"
        );
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The top-bar "Commands" menu opens the database picker and the save form.
    #[test]
    fn menu_commands_open_overlays() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("menucmd");
        let (cfg, dbpath) = config_with_temp_db("menucmd");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");
        app.open_inbox();
        assert!(app.menu_open, "inbox opened");

        app.apply_menu_action(WsAction::SavedCommands);
        assert!(app.command_picker_ref().is_some(), "picker opened from menu");
        app.command_picker = None;

        app.apply_menu_action(WsAction::NewCommand);
        assert!(app.command_form_mut().is_some(), "form opened from menu");
        app.command_form = None;

        app.apply_menu_action(WsAction::ImportCheat);
        assert!(
            matches!(app.prompt().map(|p| p.kind), Some(PromptKind::CheatSheet)),
            "import opens the cheat.sh topic prompt"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// J/K (and shift+up/down) in the inbox reorder workspaces: the order
    /// changes, `current` follows its workspace, the menu stays open on the
    /// moved row, and the new order hits the layout file.
    #[test]
    fn inbox_move_reorders_workspaces() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("movews");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2
        let names = |a: &App| {
            a.workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        app.switch_to(0);
        app.open_inbox();
        assert!(app.menu_open);

        // Row 0 (main) moves down with J; current follows it.
        app.handle_key(key(Key::Char('J'), Mods::empty()));
        assert!(app.menu_open, "menu stays open after a move");
        assert_eq!(names(&app), ["ws-1", "main", "ws-2"]);
        assert_eq!(app.current, 1);
        assert_eq!(app.workspaces[app.current].name, "main");
        assert!(
            matches!(
                app.menu.highlight().and_then(|i| i.data.clone()),
                Some(WsAction::Switch(1))
            ),
            "highlight follows the moved row"
        );
        // K moves it back up.
        app.handle_key(key(Key::Char('K'), Mods::empty()));
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        assert_eq!(app.current, 0);
        // Clamp: K at the top is a no-op (menu still open, order kept).
        app.handle_key(key(Key::Char('K'), Mods::empty()));
        assert_eq!(names(&app), ["main", "ws-1", "ws-2"]);
        assert!(app.menu_open);
        // Shift+Down moves too.
        app.handle_key(key(Key::Down, Mods::SHIFT));
        assert_eq!(names(&app), ["ws-1", "main", "ws-2"]);
        // Moving another row across `current` keeps current on its workspace.
        app.handle_key(key(Key::Char('J'), Mods::empty()));
        assert_eq!(names(&app), ["ws-1", "ws-2", "main"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        assert_eq!(app.current, 2);
        // Persistence is explicit: nothing is written until "Save layout".
        assert!(!layout.is_file(), "reorder must not auto-save");
        app.run_command(Command::SaveLayout);
        let text = std::fs::read_to_string(&layout).expect("layout saved");
        let file: crate::layout::LayoutFile = toml::from_str(&text).expect("layout parses");
        let saved: Vec<String> = file.workspaces.iter().map(|w| w.name.clone()).collect();
        assert_eq!(saved, ["ws-1", "ws-2", "main"]);

        // Plain j/k still navigate instead of moving.
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        assert_eq!(names(&app), ["ws-1", "ws-2", "main"]);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(!app.menu_open);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `move_workspace` tracks `current` through removals/insertions.
    #[test]
    fn move_workspace_keeps_current() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("movecur");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.new_workspace(); // ws-1
        app.new_workspace(); // ws-2
        app.new_workspace(); // ws-3 -> [main, ws-1, ws-2, ws-3]
        let names = |a: &App| {
            a.workspaces
                .iter()
                .map(|w| w.name.clone())
                .collect::<Vec<_>>()
        };
        // Move first to last while current sits in the middle.
        app.switch_to(1); // ws-1
        app.move_workspace(0, 3);
        assert_eq!(names(&app), ["ws-1", "ws-2", "ws-3", "main"]);
        assert_eq!(app.workspaces[app.current].name, "ws-1");
        // Move last to first while current is last.
        app.switch_to(3); // main
        app.move_workspace(3, 0);
        assert_eq!(names(&app), ["main", "ws-1", "ws-2", "ws-3"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        // No-ops and out-of-range moves change nothing.
        app.move_workspace(1, 1);
        app.move_workspace(0, 9);
        app.move_workspace(9, 0);
        assert_eq!(names(&app), ["main", "ws-1", "ws-2", "ws-3"]);
        assert_eq!(app.workspaces[app.current].name, "main");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// ctrl+p opens the palette and Enter runs the highlighted command.
    #[test]
    fn palette_runs_selected_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL));
        assert!(app.palette.is_some(), "ctrl+p opens the palette");
        // First entry is "Split horizontally".
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.palette.is_none());
        assert_eq!(app.ws().leaf_ids().len(), 2, "command executed");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The palette must offer the terminal-sharing commands (regression: they
    /// were added to `Command::ALL` and must be reachable via ctrl+p).
    #[test]
    fn palette_offers_share_commands() {
        let mut p = Palette::new();
        let labels: Vec<&str> = p.results.iter().map(|i| i.label).collect();
        assert!(
            labels.iter().any(|l| l.contains("Share terminal")),
            "palette missing Share terminal: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("Stop sharing")),
            "palette missing Stop sharing: {labels:?}"
        );
        // Fuzzy search for "share" finds it.
        for c in "share".chars() {
            p.push_char(c);
        }
        assert!(
            p.results.iter().any(|i| i.label.contains("Share terminal")),
            "typing 'share' does not match: {:?}",
            p.results.iter().map(|i| i.label).collect::<Vec<_>>()
        );
    }

    /// Typing fuzzy-filters the palette; the query is editable.
    #[test]
    fn palette_fuzzy_filters_by_typing() {
        let mut p = Palette::new();
        let all = p.results.len();
        assert!(all > 5);
        for c in "spl".chars() {
            p.push_char(c);
        }
        assert_eq!(p.query, "spl");
        assert!(p.results.len() < all, "query narrows the list");
        // Contiguous matches rank first (fuzzy is a subsequence, so e.g.
        // "Focus pane left" also matches s..p..l).
        assert!(
            p.results[0].label.contains("Split") && p.results[1].label.contains("Split"),
            "best matches first: {:?}",
            p.results.iter().map(|i| i.label).collect::<Vec<_>>()
        );
        // Typing more is case-insensitive and non-contiguous (fuzzy).
        p.backspace();
        p.backspace();
        p.backspace();
        assert_eq!(p.query, "");
        assert_eq!(p.results.len(), all, "clearing restores everything");
        for c in "pt".chars() {
            p.push_char(c);
        }
        assert!(
            p.results.iter().any(|i| i.label.contains("Split")),
            "fuzzy subsequence matches Split"
        );
    }

    /// The palette's SELECTED command runs on Enter.
    #[test]
    fn palette_enter_runs_matched_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette-run");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL));
        for c in "quit".chars() {
            app.handle_key(key(Key::Char(c), Mods::empty()));
        }
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.should_quit, "typing 'quit' then enter quits");
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Running "Save layout" from the palette writes the file straight away.
    #[test]
    fn palette_save_layout_writes_immediately() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("palette-savelayout");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        assert!(!layout.is_file(), "nothing written before the command");
        app.handle_key(key(Key::Char('p'), Mods::CONTROL)); // command palette
        for c in "save layout".chars() {
            app.handle_key(key(Key::Char(c), Mods::empty()));
        }
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(
            layout.is_file() && app.status.starts_with("layout saved"),
            "status: {}",
            app.status
        );
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Hot reload: valid edits apply; broken edits keep the old config and
    /// surface the parser's line/column in the status line.
    #[test]
    fn config_reload_applies_or_reports_error() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("reload");
        let cfg_path = unique_temp_path("cfg");
        std::fs::write(&cfg_path, "[general]\nscrollback = 1234\n").unwrap();
        let cfg = Config::load(Some(&cfg_path));
        let mut app = App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("boots");
        assert_eq!(app.config.general.scrollback, 1234);

        // Valid change applies.
        std::fs::write(&cfg_path, "[general]\nscrollback = 4321\n").unwrap();
        app.reload_config();
        assert_eq!(app.config.general.scrollback, 4321);
        assert!(app.status.contains("reloaded"));

        // Broken change is rejected; old value and an error are shown.
        std::fs::write(&cfg_path, "[general]\nscrollback = ???\n").unwrap();
        app.reload_config();
        assert_eq!(app.config.general.scrollback, 4321, "kept old config");
        assert!(app.status.contains("config error"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        let _ = std::fs::remove_file(&cfg_path);
        rt.shutdown_background();
    }

    #[test]
    fn select_mode_enters_and_yanks() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("select");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");

        let mut mods = Mods::empty();
        mods.insert(Mods::CONTROL);
        mods.insert(Mods::SHIFT);
        app.handle_key(KeyPress {
            key: Key::Space,
            mods,
            text: None,
            kind: KeyKind::Press,
        });
        assert!(app.select.is_some(), "ctrl+shift+space enters select mode");

        // Home then End stay on the same row.
        app.handle_key(key(Key::Home, Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().1, 0);
        app.handle_key(key(Key::End, Mods::empty()));
        let row = app.select.as_ref().unwrap().cursor().0;
        assert_eq!(app.select.as_ref().unwrap().cursor().1, 79);
        app.handle_key(key(Key::Home, Mods::empty()));
        assert_eq!(app.select.as_ref().unwrap().cursor().0, row);

        // Anchor with v, move down, yank with y -> mode exits.
        app.handle_key(key(Key::Char('v'), Mods::empty()));
        assert!(app.select.as_ref().unwrap().range().is_some());
        app.handle_key(key(Key::Char('j'), Mods::empty()));
        app.handle_key(key(Key::Char('y'), Mods::empty()));
        assert!(app.select.is_none(), "y leaves select mode");
        assert!(app.status.starts_with("yanked") || app.status.contains("yank"));

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Mouse routing: ignored without tracking, forwarded with it, gated by
    /// modal state and the `[mouse]` switch.
    #[test]
    fn mouse_button_move_wheel_routing() {
        use crate::mouse::MouseButton as MButton;

        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mouse");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Fill scrollback so wheel scrolling has somewhere to go.
        let mut fill = String::new();
        for i in 0..30 {
            fill.push_str(&format!("line{i:02}\r\n"));
        }
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(fill.as_bytes());
        // Full-area (5,5) -> inner grid (4,4).
        let press = |app: &mut App| {
            app.mouse_button(5, 5, MButton::Left, true, Mods::empty())
        };

        // No tracking: clicks are ignored here (caller still focuses).
        assert_eq!(press(&mut app), MouseClickOutcome::Ignored);
        assert!(!app.mouse_move(5, 5, Mods::empty(), false));
        assert!(!app.mouse_move(5, 5, Mods::empty(), true));
        // Wheel without tracking scrolls back instead (returns false).
        assert!(!app.mouse_wheel(5, 5, 3, Mods::empty()));
        let sb = app
            .ws()
            .pane(fid)
            .expect("pane")
            .screen()
            .scrollback();
        assert_eq!(sb, 3);

        // App enables tracking: presses forward, releases too (1000 covers
        // both), motion stays off without 1003.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(press(&mut app), MouseClickOutcome::Forwarded);
        assert_eq!(
            app.mouse_button(5, 5, MButton::Left, false, Mods::empty()),
            MouseClickOutcome::Forwarded
        );
        assert!(!app.mouse_move(5, 5, Mods::empty(), true));
        // Wheel is forwarded now (returns true); writing to the pane
        // jumps back to the live screen, clearing the 3 above.
        assert!(app.mouse_wheel(5, 5, 3, Mods::empty()));
        let sb2 = app
            .ws()
            .pane(fid)
            .expect("pane")
            .screen()
            .scrollback();
        assert_eq!(sb2, 0, "write() returns to the live screen");

        // Any-motion mode reports free moves as well.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1003h");
        assert!(app.mouse_move(5, 5, Mods::empty(), false));

        // An open menu consumes pane clicks (dismiss) instead of
        // forwarding them; the `[mouse] enabled = false` switch gates
        // everything else back to Ignored.
        app.menu_open = true;
        assert_eq!(press(&mut app), MouseClickOutcome::Menu);
        assert!(!app.menu_open, "pane click dismisses the menu");
        app.config.mouse.enabled = false;
        assert_eq!(press(&mut app), MouseClickOutcome::Ignored);
        assert!(!app.mouse_move(5, 5, Mods::empty(), false));
        app.config.mouse.enabled = true;

        // Modifier+click on empty space falls through (no link, no browser).
        let mut ctrl = Mods::empty();
        ctrl.insert(Mods::CONTROL);
        assert_eq!(
            app.mouse_button(5, 5, MButton::Left, true, ctrl),
            MouseClickOutcome::Forwarded
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// MCP clicks are pane-relative: they map onto the inner grid (border
    /// inset) and forward only when the pane's app tracks the mouse.
    #[test]
    fn mcp_mouse_click_translates_pane_coords() {
        use crate::mcp::McpReply;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mcpclick");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 1, 80, 24))]);

        // Without tracking the click is rejected with a helpful message.
        match app.mcp_mouse_click(fid, 3, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Err(e) => assert!(e.contains("mouse tracking"), "err: {e}"),
            _ => panic!("expected rejection without tracking"),
        }

        // Out-of-grid coordinates are rejected before any hit-test.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        match app.mcp_mouse_click(fid, 200, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Err(e) => assert!(e.contains("outside the pane grid"), "err: {e}"),
            _ => panic!("expected out-of-grid rejection"),
        }

        // In-bounds: press+release forwarded, reported by cell.
        match app.mcp_mouse_click(fid, 3, 2, crate::mouse::MouseButton::Left, None) {
            McpReply::Text(t) => assert!(t.contains("(3, 2)"), "text: {t}"),
            _ => panic!("expected a click report"),
        }

        // Drag with a non-left button: press, motion along the line, release.
        match app.mcp_mouse_drag(
            fid,
            (1, 1),
            (5, 4),
            crate::mouse::MouseButton::Right,
            None,
        ) {
            McpReply::Text(t) => assert!(t.contains("dragged Right"), "drag: {t}"),
            _ => panic!("expected a drag report"),
        }

        // An out-of-grid drag end is rejected before any bytes go out.
        match app.mcp_mouse_drag(
            fid,
            (1, 1),
            (200, 4),
            crate::mouse::MouseButton::Left,
            None,
        ) {
            McpReply::Err(e) => assert!(e.contains("outside the pane grid"), "err: {e}"),
            _ => panic!("expected out-of-grid drag rejection"),
        }

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Holding the button no longer streams identical motion reports: a
    /// repeat move in the same cell is coalesced, a new cell reports, and a
    /// fresh button gesture resets the cache.
    #[test]
    fn mouse_motion_is_coalesced_per_cell() {
        use crate::mouse::MouseButton as MButton;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mousecoalesce");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Drag tracking (1002) + SGR.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[?1002h\x1b[?1006h");

        // First drag report at a cell is sent; an immediate repeat is not.
        assert!(app.mouse_move(5, 5, Mods::empty(), true), "first move reports");
        assert!(
            !app.mouse_move(5, 5, Mods::empty(), true),
            "same cell is coalesced"
        );
        // A different cell reports again.
        assert!(app.mouse_move(6, 5, Mods::empty(), true), "new cell reports");
        // Releasing and pressing again resets the cache, so the next move
        // at the very same cell reports.
        app.mouse_button(6, 5, MButton::Left, false, Mods::empty());
        app.mouse_button(6, 5, MButton::Left, true, Mods::empty());
        assert!(
            app.mouse_move(6, 5, Mods::empty(), true),
            "gesture reset re-reports"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// `url_at` finds a fed link at inner-grid coords without opening
    /// anything (opening a real browser is never exercised in tests).
    #[test]
    fn mouse_url_at_finds_fed_link() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mouseurl");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let fid = app.ws().focused;
        app.ws_mut()
            .set_rects(&[(fid, ratatui::layout::Rect::new(0, 0, 80, 24))]);
        // Parser untouched by the live shell so far (no polls): home the
        // cursor and write one link row deterministically.
        app.ws_mut()
            .pane_mut(fid)
            .expect("pane")
            .feed_for_test(b"\x1b[Hhttps://example.com/x");
        // Full-area (1,1) is inner grid (0,0): the link starts there.
        assert_eq!(
            app.url_at(1, 1),
            Some("https://example.com/x".to_string())
        );
        assert_eq!(app.url_at(10, 1), Some("https://example.com/x".to_string()));
        assert_eq!(app.url_at(1, 5), None);
        // Default url_mod is ctrl; alt also parses via config.
        let mut alt = Mods::empty();
        alt.insert(Mods::ALT);
        assert_eq!(app.url_mods(), Mods::CONTROL);
        app.config.mouse.url_mod = "alt".to_string();
        assert_eq!(app.url_mods(), Mods::ALT);
        app.config.mouse.url_mod = "bogus".to_string();
        assert_eq!(app.url_mods(), Mods::CONTROL);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The keymap routes config bindings to commands (single source of truth).
    #[test]
    fn keymap_routes_bindings_to_commands() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("keymap");
        let app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let ctrl = |c: char| KeyPress {
            key: Key::Char(c),
            mods: Mods::CONTROL,
            text: None,
            kind: KeyKind::Press,
        };
        assert!(matches!(
            app.lookup_command(&ctrl('o')),
            Some(Command::Inbox)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('p')),
            Some(Command::CommandPalette)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('e')),
            Some(Command::EditConfig)
        ));
        assert!(matches!(
            app.lookup_command(&ctrl('y')),
            Some(Command::YankLast)
        ));
        // Unbound key -> no command (falls through to the terminal).
        assert!(app.lookup_command(&ctrl('z')).is_none());
        let mut app = app;
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// The state pattern reports the right modal owner.
    #[test]
    fn mode_reflects_active_overlay() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mode");
        let mut app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        assert_eq!(app.mode(), Mode::Normal);
        app.open_palette();
        assert_eq!(app.mode(), Mode::Palette);
        app.palette = None;
        app.prompt = Some(Prompt::open(PromptKind::AskAi));
        assert_eq!(app.mode(), Mode::Prompt);
        app.prompt = None;
        app.toggle_select();
        assert_eq!(app.mode(), Mode::Select);
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Window title reflects the focused pane and workspace.
    #[test]
    fn window_title_names_focus() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("title");
        let app = App::new(Config::default(), Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");
        let title = app.window_title();
        assert!(title.starts_with("termrs - "), "title: {title}");
        assert!(title.contains("main"), "title: {title}");
        let mut app = app;
        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }

    /// Config whose command database is a throwaway temp file.
    fn config_with_temp_db(tag: &str) -> (Config, std::path::PathBuf) {
        let db = unique_temp_path(tag).with_extension("db");
        let mut cfg = Config::default();
        cfg.commands.db = db.to_string_lossy().into_owned();
        (cfg, db)
    }

    fn remove_db(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    /// Type text into whichever overlay owns the keyboard.
    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            let k = if c == ' ' { Key::Space } else { Key::Char(c) };
            app.handle_key(key(k, Mods::empty()));
        }
    }

    /// Picking a saved command without placeholders types it and runs it.
    #[test]
    fn command_picker_runs_saved_command() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdpick");
        let (cfg, dbpath) = config_with_temp_db("cmdpick");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("echo hi", "greet", "")
            .unwrap();

        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_some(), "ctrl+r opens picker");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_args.is_none());
        assert!(app.status.starts_with("run: echo hi"), "status: {}", app.status);
        assert_eq!(app.db.as_ref().unwrap().all().unwrap()[0].uses, 1);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// A command with `{name}` placeholders opens a form and fills them in,
    /// quoting values that contain spaces.
    #[test]
    fn placeholder_command_prompts_then_fills() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdargs");
        let (cfg, dbpath) = config_with_temp_db("cmdargs");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("cp {src} {dst}", "copy", "")
            .unwrap();
        app.open_commands();
        app.handle_key(key(Key::Enter, Mods::empty()));
        {
            let args = app.command_args.as_ref().expect("placeholder form");
            assert_eq!(args.names, vec!["src", "dst"]);
        }
        type_text(&mut app, "a b.txt");
        app.handle_key(key(Key::Tab, Mods::empty()));
        type_text(&mut app, "out");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_args.is_none());
        assert!(app.status.contains("\"a b.txt\""), "status: {}", app.status);
        assert!(app.status.starts_with("run: cp"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// The save form (ctrl+shift+r) persists command + comment.
    #[test]
    fn command_save_form_persists() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdsave");
        let (cfg, dbpath) = config_with_temp_db("cmdsave");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false).expect("app boots");

        app.handle_key(key(Key::Char('r'), Mods::CONTROL | Mods::SHIFT));
        assert!(app.command_form_mut().is_some(), "ctrl+shift+r opens form");
        assert!(app.command_picker_ref().is_none(), "not the picker");
        type_text(&mut app, "git log --oneline -n {count}");
        app.handle_key(key(Key::Tab, Mods::empty()));
        type_text(&mut app, "recent commits");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on save");

        let all = app.db.as_ref().unwrap().all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].command, "git log --oneline -n {count}");
        assert_eq!(all[0].comment, "recent commits");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e in the picker opens the edit table for the highlighted row;
    /// Enter updates it in place (tags and use counter kept) and Esc
    /// returns to the picker.
    #[test]
    fn command_picker_ctrl_e_edits_selected() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdedit");
        let (cfg, dbpath) = config_with_temp_db("cmdedit");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
                .expect("app boots");
        let id = app
            .db
            .as_ref()
            .unwrap()
            .add("git log", "recent commits", "git")
            .unwrap();
        app.db.as_ref().unwrap().mark_used(id).unwrap();

        // Esc from the edit table returns to the picker.
        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_some(), "picker opens");
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        assert!(app.command_picker_ref().is_none(), "picker closed");
        let form = app.command_form_mut().expect("edit table opens");
        assert_eq!(form.edit_id, Some(id));
        assert_eq!(form.command_text(), "git log");
        assert_eq!(form.comment_text(), "recent commits");
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on esc");
        assert!(app.command_picker_ref().is_some(), "esc returns to picker");

        // Re-open, edit the command, Enter updates it in place.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        type_text(&mut app, " --oneline");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed on save");
        let row = &app.db.as_ref().unwrap().all().unwrap()[0];
        assert_eq!(row.command, "git log --oneline");
        assert_eq!(row.comment, "recent commits");
        assert_eq!(row.tags, "git", "tags kept");
        assert_eq!(row.uses, 1, "use counter kept");
        assert!(app.status.contains("updated command"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e with a non-empty filter types `e` into the query instead of
    /// opening the edit table (fuzzy search keeps working).
    #[test]
    fn command_picker_plain_e_types_into_filter() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cmdetype");
        let (cfg, dbpath) = config_with_temp_db("cmdetype");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
                .expect("app boots");
        app.db.as_ref().unwrap().add("git log", "commits", "").unwrap();

        app.handle_key(key(Key::Char('r'), Mods::CONTROL));
        type_text(&mut app, "e");
        assert!(app.command_picker_ref().is_some(), "picker stays open");
        assert!(app.command_form_mut().is_none(), "no edit table");
        assert_eq!(app.command_picker_ref().expect("picker").query, "e");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// A finished cheat.sh import opens the tick-box picker with fresh rows
    /// unticked (dupes skipped); space ticks, Enter adds tagged rows.
    #[test]
    fn cheat_import_opens_picker_and_adds_ticked() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatimport");
        let (cfg, dbpath) = config_with_temp_db("cheatimport");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
                .expect("app boots");
        app.db
            .as_ref()
            .unwrap()
            .add("tar -czvf a.tgz dir", "old", "")
            .unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(Ok(CheatImport {
            topic: "tar".into(),
            entries: vec![
                crate::cheatsheet::CheatEntry {
                    command: "tar -xvf {archive}".into(),
                    comment: "extract".into(),
                },
                crate::cheatsheet::CheatEntry {
                    command: "tar -czvf a.tgz dir".into(),
                    comment: "create".into(),
                },
            ],
        }))
        .unwrap();
        app.cheat_rx = Some(rx);
        app.poll_cheat();
        assert!(app.cheat_rx.is_none(), "channel consumed");
        assert_eq!(app.db.as_ref().unwrap().all().unwrap().len(), 1, "nothing saved yet");
        let picker = app.cheat_picker_ref().expect("review picker opens");
        assert_eq!(picker.results.len(), 1, "dupe filtered");
        assert_eq!(picker.checked_count(), 0, "starts unticked");
        assert!(app.status.contains("already saved"), "status: {}", app.status);

        // Empty Enter keeps it open; space ticks, Enter adds.
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.cheat_picker_ref().is_some(), "empty submit keeps picker");
        app.handle_key(key(Key::Space, Mods::empty()));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 1);
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.cheat_picker_ref().is_none(), "submit closes picker");
        let all = app.db.as_ref().unwrap().all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|c| c.tags == "cheat.sh:tar"), "tagged: {all:?}");
        assert!(app.status.contains("added 1 commands"), "status: {}", app.status);
        assert!(app.command_picker_ref().is_some(), "command picker opens after");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Ctrl+e in the import picker opens the edit table for the highlighted
    /// row; Enter rewrites the pending row (nothing saved), Esc returns.
    #[test]
    fn cheat_picker_ctrl_e_edits_row() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatedit");
        let (cfg, dbpath) = config_with_temp_db("cheatedit");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
                .expect("app boots");
        app.cheat_picker = Some(CheatPicker::new(
            "tar".into(),
            vec![
                crate::cheatsheet::CheatEntry {
                    command: "tar -xvf {archive}".into(),
                    comment: "extract".into(),
                },
                crate::cheatsheet::CheatEntry {
                    command: "tar -tzvf {archive}".into(),
                    comment: "list".into(),
                },
            ],
        ));

        // Ctrl+e on the first row opens the edit table.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        let form = app.command_form_mut().expect("edit table opens");
        assert_eq!(form.edit_cheat, Some(0));
        assert_eq!(form.origin, FormOrigin::CheatPicker);
        assert_eq!(form.command_text(), "tar -xvf {archive}");
        assert_eq!(form.comment_text(), "extract");

        // Modify command + comment (cursor starts at end), Enter rewrites
        // the pending row.
        type_text(&mut app, " extra");
        app.handle_key(key(Key::Tab, Mods::empty()));
        assert_eq!(app.command_form_mut().expect("form").field, 1);
        type_text(&mut app, " done");
        app.handle_key(key(Key::Enter, Mods::empty()));
        assert!(app.command_form_mut().is_none(), "form closed");
        let picker = app.cheat_picker_ref().expect("import picker still open");
        assert_eq!(picker.items[0].entry.command, "tar -xvf {archive} extra");
        assert_eq!(picker.items[0].entry.comment, "extract done");
        // Nothing reached the database.
        assert!(app.db.as_ref().unwrap().all().unwrap().is_empty());
        assert!(app.status.contains("edited import row"), "status: {}", app.status);

        // Esc from the edit table returns to the import picker.
        app.handle_key(key(Key::Char('e'), Mods::CONTROL));
        assert!(app.command_form_mut().is_some());
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.command_form_mut().is_none());
        assert!(app.cheat_picker_ref().is_some(), "esc returns to import picker");

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Select-all / clear-all / Esc in the import review picker.
    #[test]
    fn cheat_picker_select_all_clear_and_cancel() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("cheatpick");
        let (cfg, dbpath) = config_with_temp_db("cheatpick");
        let mut app =
            App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
                .expect("app boots");
        app.cheat_picker = Some(CheatPicker::new(
            "tar".into(),
            vec![
                crate::cheatsheet::CheatEntry { command: "tar -xvf {a}".into(), comment: String::new() },
                crate::cheatsheet::CheatEntry { command: "tar -tzvf {a}".into(), comment: String::new() },
            ],
        ));
        app.handle_key(key(Key::Char('a'), Mods::CONTROL));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 2);
        app.handle_key(key(Key::Char('u'), Mods::CONTROL));
        assert_eq!(app.cheat_picker_ref().expect("picker").checked_count(), 0);
        app.handle_key(key(Key::Esc, Mods::empty()));
        assert!(app.cheat_picker_ref().is_none());
        assert!(app.db.as_ref().unwrap().all().unwrap().is_empty());
        assert!(app.status.contains("cancelled"), "status: {}", app.status);

        for w in &mut app.workspaces {
            w.kill_all();
        }
        rt.shutdown_background();
        remove_db(&dbpath);
        let _ = std::fs::remove_file(&layout);
    }

    /// Unique temp layout path per test invocation, so a panicking run can
    /// never pollute the next one with a stale file.
    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "termrs-test-{tag}-{}-{nanos}.toml",
            std::process::id()
        ))
    }

    /// A pane-scoped MCP server driving the real App: open a server on the
    /// focused pane, then call a tool over HTTP while the UI thread answers
    /// pane queries (which only happens inside `poll_panes`).
    #[test]
    fn mcp_pane_serves_real_pane_queries() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let layout = unique_temp_path("mcp");
        let mut cfg = Config::default();
        cfg.general.mcp_port = 0; // ephemeral: no clash with a real server
        let mut app = App::new(cfg, Some(layout.clone()), rt.handle(), &wake, None, None, false)
            .expect("app boots");

        app.open_mcp_pane(None);
        let pane_id = app.ws().focused;
        let port = app
            .mcp_servers
            .get(&pane_id)
            .expect("MCP server attached")
            .port;

        // Type a unique marker so the screen query must read real pane state.
        app.ws_mut()
            .pane_mut(pane_id)
            .expect("pane")
            .write(b"echo mcp-probe-123\r");
        let mut seen = false;
        for _ in 0..80 {
            app.poll_panes();
            let hit = app
                .ws()
                .pane(pane_id)
                .map(|p| p.screen().contents().contains("mcp-probe-123"))
                .unwrap_or(false);
            if hit {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "shell produced the probe line");

        // Drive MCP on a worker thread; the UI thread must keep polling to
        // answer the tool's pane query.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let url = format!("http://127.0.0.1:{port}/mcp");
            let post = |body: &str, session: Option<&str>| -> (String, Option<String>) {
                let mut req = ureq::post(&url)
                    .set("Content-Type", "application/json")
                    .set("Accept", "application/json, text/event-stream")
                    .set("MCP-Protocol-Version", "2025-11-25");
                if let Some(s) = session {
                    req = req.set("Mcp-Session-Id", s);
                }
                let resp = req.send_string(body).expect("response");
                let sid = resp.header("mcp-session-id").map(str::to_string);
                (resp.into_string().unwrap_or_default(), sid)
            };
            let init = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" }
                }
            })
            .to_string();
            let (_, session) = post(&init, None);
            let session = session.expect("session id");
            let ready = serde_json::json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            })
            .to_string();
            let _ = post(&ready, Some(&session));
            let call = serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "termrs_get_screen", "arguments": {} }
            })
            .to_string();
            let (out, _) = post(&call, Some(&session));
            let _ = done_tx.send(out);
        });

        let mut screen = None;
        for _ in 0..400 {
            app.poll_panes();
            if let Ok(v) = done_rx.try_recv() {
                screen = Some(v);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        let screen = screen.expect("MCP get_screen completed");
        assert!(
            screen.contains("mcp-probe-123"),
            "MCP screen did not contain the probe: {screen}"
        );

        for w in &mut app.workspaces {
            w.kill_all();
        }
        let _ = std::fs::remove_file(&layout);
        rt.shutdown_background();
    }
}
