//! Scrollback viewer: full vt100 history of the focused pane.
//!
//! The live pane only shows the current screen; this overlay reconstructs the
//! retained history (oldest first) by walking a cloned screen through its
//! scrollback offsets, then presents it in a `tui-scrollview` popup with
//! line/column scrolling. Snapshot is taken on open; `r` re-snapshots.

use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, StatefulWidget};
use tui_scrollview::{ScrollView, ScrollViewState};

use crate::pty::Pane;

/// Snapshot viewer over one pane's history.
pub struct ScrollbackView {
    lines: Vec<String>,
    width: u16,
    state: ScrollViewState,
}

impl ScrollbackView {
    /// Snapshot `pane`: scrollback history (oldest first) + visible screen.
    pub fn open(pane: &Pane) -> Self {
        let mut v = Self {
            lines: Vec::new(),
            width: 1,
            state: ScrollViewState::new(),
        };
        v.refresh(pane);
        v.state.scroll_to_bottom();
        v
    }

    /// Re-snapshot (history grew while the viewer was open).
    pub fn refresh(&mut self, pane: &Pane) {
        let (lines, width) = snapshot(pane);
        self.lines = lines;
        self.width = width;
    }

    /// Apply an externally taken snapshot (avoids borrowing the pane twice).
    pub fn set_snapshot(&mut self, lines: Vec<String>, width: u16) {
        self.lines = lines;
        self.width = width;
    }

    /// Route one navigation key. True when consumed.
    pub fn input(&mut self, key: crate::keys::Key) -> bool {
        use crate::keys::Key::*;
        match key {
            Up => self.state.scroll_up(),
            Down => self.state.scroll_down(),
            Left => self.state.scroll_left(),
            Right => self.state.scroll_right(),
            PageUp => self.state.scroll_page_up(),
            PageDown => self.state.scroll_page_down(),
            Home => self.state.scroll_to_top(),
            End => self.state.scroll_to_bottom(),
            _ => return false,
        }
        true
    }

    /// Render popup content into `area` (caller centers + titles).
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" scrollback (arrows/pgup/pgdn/home/end, r=refresh, esc=close) ")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let height = self.lines.len().max(1) as u16;
        let mut view = ScrollView::new(Size::new(self.width.max(1), height));
        view.render_widget(
            Paragraph::new(self.lines.join("\n")),
            Rect::new(0, 0, self.width.max(1), height),
        );
        view.render(inner, frame.buffer_mut(), &mut self.state);
    }
}

/// Full history text: scrollback (oldest first) + current visible rows.
/// Public so callers holding a `&Pane` can snapshot without double-borrowing.
pub fn snapshot(pane: &Pane) -> (Vec<String>, u16) {
    let src = pane.screen();
    let (rows, cols) = src.size();
    let mut buf = src.clone();
    // Clamp trick: total retained scrollback lines.
    buf.set_scrollback(usize::MAX);
    let total = buf.scrollback();
    let mut lines = Vec::with_capacity(total + rows as usize);
    // Each offset's top row is one distinct history line, oldest first.
    for off in (1..=total).rev() {
        buf.set_scrollback(off);
        if let Some(first) = buf.rows(0, cols).next() {
            lines.push(first.trim_end().to_string());
        }
    }
    buf.set_scrollback(0);
    for row in buf.rows(0, cols).take(rows as usize) {
        lines.push(row.trim_end().to_string());
    }
    let width = lines
        .iter()
        .map(|l| l.chars().count().min(u16::MAX as usize) as u16)
        .max()
        .unwrap_or(1)
        .max(1);
    (lines, width)
}
