//! Ratatui rendering, based on `tui-term`'s `PseudoTerminal` widget.
//!
//! Each pane's `vt100::Screen` is rendered via `tui_term::widget::PseudoTerminal`
//! (ANSI parsing, colors, cursor all handled there). This module owns the
//! split-tree layout, focused-pane chrome, the top status bar (workspace tabs,
//! a `throbber-widgets-tui` liveness spinner), and one-shot `tachyonfx`
//! transitions for splits (dissolve-in) and focus changes (fade flash).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, StatefulWidget, Wrap};
use tachyonfx::{CellFilter, EffectTimer, Interpolation, fx};
use throbber_widgets_tui::Throbber;
use tui_big_text::{BigText, PixelSize};
use tui_menu::Menu;
use tui_term::widget::PseudoTerminal;

use crate::config::CursorShape;

use crate::app::App;
use crate::config::FxKind;
use crate::image_gpu::Dot;
use crate::workspace::{Node, SplitDir};

use std::sync::Arc;
use std::sync::Mutex;

/// Draw whole app: menu bar, panes, prompt, overlays, fx transitions.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let tick = app.frame_tick();
    let area = frame.area();
    // Menu bar, panes, optional textarea prompt, transient status line.
    let prompt_open = app.prompt().is_some();
    let status_open = !app.status.is_empty();
    let mut rows = vec![Constraint::Length(1), Constraint::Min(1)];
    if prompt_open {
        rows.push(Constraint::Length(4));
    }
    if status_open {
        rows.push(Constraint::Length(1));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(rows)
        .split(area);
    let (bar, main) = (chunks[0], chunks[1]);
    let mut idx = 2;
    let prompt_area = prompt_open.then(|| {
        let a = chunks[idx];
        idx += 1;
        a
    });
    let status_area = status_open.then(|| chunks[idx]);

    // Assign rects, then resize PTYs to fit. A zoomed pane takes the whole
    // area; the split tree underneath is untouched so unzoom restores it.
    // (The id must still exist; remove_leaf clears zoom, this is backup.)
    // Tiled rects are always computed too: directional focus reads them when
    // a focus key restores a maximized pane and moves in one press.
    let mut rects = Vec::new();
    let zoomed = app
        .ws()
        .zoomed()
        .filter(|id| app.ws().pane(*id).is_some());
    if let Some(zid) = zoomed {
        rects.push((zid, main));
    } else if let Some(root) = app.ws().root() {
        collect_rects(root, main, &mut rects);
    }
    let mut tiled = Vec::new();
    if let Some(root) = app.ws().root() {
        collect_rects(root, main, &mut tiled);
    }
    app.ws_mut().set_rects(&rects);
    app.ws_mut().set_tiled_rects(&tiled);
    let focused = app.ws().focused;
    let theme = app.theme_colors();
    // Snapshot the shared braille dot list and clear it for this frame; the
    // per-pane pass below refills it while blanking the braille font glyphs.
    let braille = app.braille.clone();
    if let Ok(mut b) = braille.lock() {
        b.clear();
    }
    for (id, rect) in &rects {
        let inner = Block::default().borders(Borders::ALL).inner(*rect);
        if inner.width >= 2 && inner.height >= 2
            && let Some(p) = app.ws_mut().pane_mut(*id) {
                p.resize(inner.width, inner.height);
            }
    }

    // Render panes (re-borrow after resize).
    for (id, rect) in &rects {
        let Some(pane) = app.ws().pane(*id) else { continue };
        let is_focused = *id == focused;
        let title = if pane.dead {
            format!(" {} [exited] ", pane.id)
        } else if is_focused {
            format!(" {} ● {} ", pane.id, pane.title)
        } else {
            format!(" {} ", pane.id)
        };
        // Persistent zoom indicator on the maximized pane.
        let title = if app.ws().zoomed() == Some(*id) {
            format!("{title}[zoom] ")
        } else {
            title
        };
        let accent = theme
            .accent
            .map(|[r, g, b]| Color::Rgb(r, g, b))
            .unwrap_or(Color::Yellow);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if is_focused {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let term = PseudoTerminal::new(pane.screen()).block(block);
        frame.render_widget(term, *rect);
        // Solid-block normalization: the GPU backend rasterizes `█` as a
        // font glyph, whose outline rarely reaches the exact cell edges, so
        // stacked blocks (progress gauges, solid art) show thin grid lines.
        // Real terminals draw blocks as fills; emulate that here by turning
        // a full block into a space painted with the glyph's color. Only the
        // hosted pane area is touched, never shellrs's own chrome.
        {
            let inner = Block::default().borders(Borders::ALL).inner(*rect);
            normalize_block_cells(frame.buffer_mut(), inner, theme);
        }

        // The wgpu backend does not draw a cursor. A `block` cursor is painted
        // into the cell buffer here; `bar`/`underline` are drawn as thin GPU
        // quads by the post-process pass (window::sync_cursor).
        if is_focused && !pane.dead {
            let (cr, cc) = pane.screen().cursor_position();
            let inner = Block::default().borders(Borders::ALL).inner(*rect);
            if !pane.screen().hide_cursor()
                && cc < inner.width
                && cr < inner.height
                && theme.cursor_shape == CursorShape::Block
            {
                frame.set_cursor_position((inner.x + cc, inner.y + cr));
                paint_cursor(frame, inner.x + cc, inner.y + cr, theme);
            }
        }
    }

    render_menu_bar(frame, app, bar);
    render_palette(frame, app, area);
    render_command_picker(frame, app, area);
    render_cheat_picker(frame, app, area);
    render_font_picker(frame, app, area);
    render_command_form(frame, app, area);
    render_command_args(frame, app, area);
    if let Some(pa) = prompt_area {
        render_prompt(frame, app, pa);
    }
    if let Some(sa) = status_area {
        render_status(frame, app, sa);
    }
    render_selection(frame, app, &rects, focused, theme);
    fire_pending_fx(app, &rects);

    // Modal scrollback overlay (full history of focused pane).
    if app.scrollback_mut().is_some() {
        let popup = centered(area, 90, 85);
        if let Some(v) = app.scrollback_mut() {
            v.render(frame, popup);
        }
    }

    // Big-text help overlay.
    if app.about {
        render_about(frame, app, centered(area, 80, 70));
    }

    // Image overlay: draw its chrome here; the texture itself is drawn by the
    // GPU post-process pass (crisp, full resolution) at the same rect.
    if let Some(img) = app.image_ref() {
        // Aspect-fitted rect from the window host (computed with real pixel
        // metrics just before this draw); falls back to the plain max box.
        let popup = app.image_rect().unwrap_or_else(|| image_popup(area));
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " {} ({}x{}, esc=close) ",
                img.path, img.dims.0, img.dims.1
            ))
            .border_style(Style::default().fg(Color::Magenta));
        frame.render_widget(block, popup);
    }

    // Run transitions on top of the rendered frame.
    if app.effects.is_running() {
        app.effects.process_effects(tick, frame.buffer_mut(), area);
    }

    // Braille cells are drawn as GPU dot quads instead of font glyphs (fonts
    // leave inconsistent rows/columns gaps). Scanning the final buffer means
    // overlays that cover a pane also suppress the dots they hide.
    collect_braille(frame.buffer_mut(), area, area, &braille, theme.fg);
}

/// Big-text help card (tui-big-text banner + key reference).
fn render_about(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(Clear, area);
    let k = &app.config.keys;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help (f1/esc to close) ")
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(1)])
        .split(inner);
    let banner = BigText::builder()
        .pixel_size(PixelSize::Full)
        .style(Style::default().fg(Color::Cyan))
        .centered()
        .lines(vec![Line::from("SHELLRS")])
        .build();
    frame.render_widget(banner, rows[0]);
    let hints = format!(
        "panes     : {} split-h | {} split-v | {} close | {} zoom | {}/{} focus\n\
         workspace : {} new | {}/{} prev/next | {} inbox menu\n\
         view      : {} scrollback | {} image | {} this help | {} quit\n\
         copy      : {} yank last command output | {} select mode\n\
         mouse     : click focuses a pane | wheel scrolls scrollback\n\
         mouse     : apps with tracking get events | ctrl+click opens URLs\n\
         paste     : {} paste | {} palette (type to filter) | {} edit config in pane\n\
          commands  : {} saved-command picker | {} save a command (comments + {{name}} placeholders)\n\
          font      : palette > \"Change font...\" (installed fonts, writes [general] font)\n\
         prompt    : Ctrl+V pastes a path | Tab accepts ghost completion\n\
         select    : h/j/k/l move | v anchor | ; swap ends | y yank | esc\n\
         select-f  : f<char> find forward | F<char> find backward\n\
         select-m  : miw/maw word | mi(/ma( parens | mi\"/ma\" quotes\n\
         select-g  : gh line-start | gl last char | gg top | ge bottom\n\
         ai        : palette > \"Ask AI\" (set [ai] command in config)\n\
         import    : palette/menu > \"Import commands from cheat.sh\" (tick, ctrl+e edit, enter add)\n\n\
         kalk, lp and piper are run as normal commands inside a pane.\n\
         Each action accepts several keys: quit = [\"ctrl+q\", \"alt+q\"].\n\
          The inbox lists every workspace with live status: alive panes, last\n\
          finished command, and a dot when background output is unread.\n\
          In the inbox, J/K (or shift+up/down) reorder workspaces.\n\
         [fx]: enabled = false kills all transitions; open/close/focus pick\n\
         dissolve|fade|flash|none plus open_ms/close_ms/focus_ms.\n\
         [window]: background_image backdrop, opacity = backdrop-layer alpha\n\
         only (text stays fully opaque), decorations = false = borderless.\n\
         Layout file: {}\n",
        k.split_horizontal.join("/"),
        k.split_vertical.join("/"),
        k.close_pane.join("/"),
        k.pane_zoom.join("/"),
        k.focus_next.join("/"),
        k.focus_prev.join("/"),
        k.workspace_new.join("/"),
        k.workspace_prev.join("/"),
        k.workspace_next.join("/"),
        k.workspace_menu.join("/"),
        k.scroll_view.join("/"),
        k.view_image.join("/"),
        k.help.join("/"),
        k.quit.join("/"),
        k.yank_last.join("/"),
        k.select_mode.join("/"),
        k.paste.join("/"),
        k.command_palette.join("/"),
        k.edit_config.join("/"),
        k.commands_picker.join("/"),
        k.command_save.join("/"),
        app.layout_path().display(),
    );
    frame.render_widget(
        Paragraph::new(hints)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(Color::White)),
        rows[1],
    );
}

/// Paint the terminal cursor cell: theme cursor color on the theme
/// background, or inverted colors when no cursor color is configured.
fn paint_cursor(frame: &mut Frame, x: u16, y: u16, theme: crate::config::ThemeColors) {
    let buf = frame.buffer_mut();
    if x >= buf.area.width || y >= buf.area.height {
        return;
    }
    let cell = &mut buf[(x, y)];
    match theme.cursor {
        Some([r, g, b]) => {
            let [br, bg, bb] = theme.bg;
            cell.set_fg(Color::Rgb(br, bg, bb)).set_bg(Color::Rgb(r, g, b));
        }
        None => {
            let (fg, bg) = (cell.fg, cell.bg);
            cell.set_fg(bg).set_bg(fg);
        }
    }
}

/// Full-block fill character: covers its whole cell by definition.
const FULL_BLOCK: &str = "█";

/// Turn `█` cells into solid background fills.
///
/// The GPU backend draws text by rasterizing font glyphs, and a font's block
/// outline almost never reaches the exact cell edges (baseline rounding,
/// side bearings, fallback-width scaling). Stacked blocks then show thin
/// grid lines: multi-row gauges stripe, halfblock art looks pixelated.
/// Real terminals (WezTerm, Kitty) draw block elements as rectangles.
///
/// A full block hides its background by definition, so replacing it with a
/// space painted in the glyph's effective color is faithful: the backend
/// draws spaces as background quads, which tile seamlessly. `Reset` colors
/// resolve against the theme (the backend maps them the same way).
/// `REVERSED` is consumed (the swap is baked into the fill); other
/// modifiers are kept. Half blocks (`▀`/`▄`) still need two colors per cell
/// and are intentionally left for the font path.
fn normalize_block_cells(
    buf: &mut ratatui::buffer::Buffer,
    area: Rect,
    theme: crate::config::ThemeColors,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let area_rect = buf.area;
    let x_end = area.x.saturating_add(area.width).min(area_rect.width);
    let y_end = area.y.saturating_add(area.height).min(area_rect.height);
    let def_fg = Color::Rgb(theme.fg[0], theme.fg[1], theme.fg[2]);
    let def_bg = Color::Rgb(theme.bg[0], theme.bg[1], theme.bg[2]);
    let resolve = |c: Color, def: Color| if c == Color::Reset { def } else { c };
    for y in area.y..y_end {
        for x in area.x..x_end {
            let cell = &mut buf[(x, y)];
            if cell.symbol() != FULL_BLOCK {
                continue;
            }
            let reversed = cell.modifier.contains(Modifier::REVERSED);
            let (fg, bg) = (cell.fg, cell.bg);
            let fill = if reversed {
                resolve(bg, def_bg)
            } else {
                resolve(fg, def_fg)
            };
            cell.set_symbol(" ").set_fg(fill).set_bg(fill);
            cell.modifier.remove(Modifier::REVERSED);
        }
    }
}

/// Fraction of a braille sub-cell a dot fills (leaves a thin, even gap like a
/// real font so rows/columns read as a grid instead of a solid slab).
const BRAILLE_DOT_FILL: f32 = 0.85;

/// Braille dot `bit` (0..=7) to its `(column, row)` inside the 2×4 cell.
fn braille_dot_cell(bit: u8) -> (u8, u8) {
    match bit {
        0 => (0, 0),
        1 => (0, 1),
        2 => (0, 2),
        3 => (1, 0),
        4 => (1, 1),
        5 => (1, 2),
        6 => (0, 3),
        _ => (1, 3),
    }
}

/// Resolve a ratatui colour to sRGB bytes (`Reset` falls back to `reset`).
fn color_rgb(c: Color, reset: [u8; 3]) -> [u8; 3] {
    match c {
        Color::Reset => reset,
        Color::Black => [0, 0, 0],
        Color::Red => [205, 0, 0],
        Color::Green => [0, 205, 0],
        Color::Yellow => [205, 205, 0],
        Color::Blue => [0, 0, 238],
        Color::Magenta => [205, 0, 205],
        Color::Cyan => [0, 205, 205],
        Color::Gray => [229, 229, 229],
        Color::DarkGray => [127, 127, 127],
        Color::LightRed => [255, 0, 0],
        Color::LightGreen => [0, 255, 0],
        Color::LightYellow => [255, 255, 0],
        Color::LightBlue => [92, 92, 255],
        Color::LightMagenta => [255, 0, 255],
        Color::LightCyan => [0, 255, 255],
        Color::White => [255, 255, 255],
        Color::Rgb(r, g, b) => [r, g, b],
        Color::Indexed(i) => indexed_rgb(i),
    }
}

/// xterm 256-colour palette lookup.
fn indexed_rgb(i: u8) -> [u8; 3] {
    const BASE: [[u8; 3]; 16] = [
        [0, 0, 0],
        [205, 0, 0],
        [0, 205, 0],
        [205, 205, 0],
        [0, 0, 238],
        [205, 0, 205],
        [0, 205, 205],
        [229, 229, 229],
        [127, 127, 127],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [92, 92, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    match i {
        0..=15 => BASE[i as usize],
        16..=231 => {
            let i = i - 16;
            let steps = [0u8, 95, 135, 175, 215, 255];
            [
                steps[(i / 36) as usize],
                steps[((i % 36) / 6) as usize],
                steps[(i % 6) as usize],
            ]
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            [v, v, v]
        }
    }
}

/// sRGB bytes to linear RGBA (the sRGB surface re-encodes on write).
fn srgb_to_linear(rgb: [u8; 3]) -> [f32; 4] {
    let f = |v: u8| {
        let c = v as f32 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    [f(rgb[0]), f(rgb[1]), f(rgb[2]), 1.0]
}

/// Replace braille cells in `buf` with GPU dot quads.
///
/// Font braille glyphs do not fill their cell (advance/line-height and glyph
/// coverage differ per font), so stacking them leaves inconsistent gaps
/// between rows. Instead each set dot becomes a small quad, placed on the
/// regular 2×4 sub-grid so the spacing is perfectly uniform in every
/// direction. The font glyph is blanked so only the quads show.
fn collect_braille(
    buf: &mut ratatui::buffer::Buffer,
    inner: Rect,
    area: Rect,
    out: &Arc<Mutex<Vec<Dot>>>,
    reset: [u8; 3],
) {
    if inner.width == 0 || inner.height == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let Ok(mut dots) = out.lock() else {
        return;
    };
    let cols = area.width as f32;
    let rows = area.height as f32;
    let buf_area = buf.area;
    let x_end = inner.x.saturating_add(inner.width).min(buf_area.width);
    let y_end = inner.y.saturating_add(inner.height).min(buf_area.height);
    for y in inner.y..y_end {
        for x in inner.x..x_end {
            let cell = &mut buf[(x, y)];
            let Some(ch) = cell.symbol().chars().next() else {
                continue;
            };
            let cp = ch as u32;
            if !(0x2800..=0x28FF).contains(&cp) {
                continue;
            }
            let pattern = (cp - 0x2800) as u8;
            let color = srgb_to_linear(color_rgb(cell.fg, reset));
            // The font glyph is replaced by the quads below.
            cell.set_symbol(" ");
            if pattern == 0 {
                continue;
            }
            for bit in 0..8u8 {
                if pattern & (1 << bit) == 0 {
                    continue;
                }
                let (dx, dy) = braille_dot_cell(bit);
                let cx = x as f32 + (dx as f32 + 0.5) * 0.5;
                let cy = y as f32 + (dy as f32 + 0.5) * 0.25;
                let ndc_x = cx / cols * 2.0 - 1.0;
                let ndc_y = 1.0 - cy / rows * 2.0;
                let hw = BRAILLE_DOT_FILL * 0.5 / cols;
                let hh = BRAILLE_DOT_FILL * 0.25 / rows;
                dots.push(Dot {
                    ndc: [ndc_x - hw, ndc_y + hh, ndc_x + hw, ndc_y - hh],
                    color,
                });
            }
        }
    }
}

/// Popup rect used for the image overlay (shared with the GPU image pass so
/// the drawn texture lines up with the border drawn here).
pub(crate) fn image_popup(area: Rect) -> Rect {
    centered(area, 80, 80)
}

/// Inner rect of the font-picker's preview box, in cells. Shared with the
/// GPU overlay pass so the rasterized preview lands inside the border.
pub(crate) fn font_preview_inner(area: Rect) -> Rect {
    let popup = centered(area, 64, 64);
    let inner = Block::default().borders(Borders::ALL).inner(popup);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(1),
        ])
        .split(inner);
    Block::default().borders(Borders::ALL).inner(rows[2])
}

/// Aspect-preserving popup rect for an image of `dims` (pixels).
///
/// The plain [`image_popup`] box is only the *maximum*: the image is fitted
/// inside its bordered inner area using real cell pixel metrics, then the
/// border is re-added around the fitted cells. Both this border and the GPU
/// texture pass (`window.rs`) use this one rect, so they line up exactly
/// and the image is never stretched.
pub(crate) fn image_popup_fitted(area: Rect, dims: (u32, u32), cell_w: f32, cell_h: f32) -> Rect {
    let outer = image_popup(area);
    let inner = Block::default().borders(Borders::ALL).inner(outer);
    let (iw, ih) = (dims.0, dims.1);
    if iw == 0
        || ih == 0
        || inner.width < 2
        || inner.height < 2
        || cell_w.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
        || cell_h.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater)
    {
        return outer;
    }
    let scale = ((inner.width as f32 * cell_w) / iw as f32)
        .min((inner.height as f32 * cell_h) / ih as f32);
    if !scale.is_finite() || scale <= 0.0 {
        return outer;
    }
    let fw = ((iw as f32 * scale) / cell_w).round() as u32;
    let fh = ((ih as f32 * scale) / cell_h).round() as u32;
    let fw = fw.clamp(1, inner.width as u32) as u16;
    let fh = fh.clamp(1, inner.height as u32) as u16;
    // Center the fitted cells inside the max box, then re-add the border.
    let fx = inner.x + (inner.width - fw) / 2;
    let fy = inner.y + (inner.height - fh) / 2;
    Rect::new(
        fx.saturating_sub(1),
        fy.saturating_sub(1),
        fw.saturating_add(2),
        fh.saturating_add(2),
    )
}

/// Centered rect covering `pct_x`% x `pct_y`% of `area`.
fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

/// Fire queued split/focus/close transitions against computed layout rects.
/// Effect name + duration come from `[fx]`; disabled/unknown/zero means no
/// effect (the pane still appears or is removed as usual).
fn fire_pending_fx(app: &mut App, rects: &[(usize, Rect)]) {
    for pend in app.drain_fx() {
        let Some(rect) = rects.iter().find(|(id, _)| *id == pend.pane).map(|(_, r)| *r) else {
            continue;
        };
        let Some(effect) = build_fx(pend.kind, &app.config.fx, rect) else {
            continue;
        };
        app.effects.add_effect(effect);
    }
}

/// Build one transition effect. `fade` is direction-aware: fade-in for fresh
/// and focus (from white), fade-out for close (to black).
fn build_fx(
    kind: FxKind,
    fx: &crate::config::FxConfig,
    rect: Rect,
) -> Option<tachyonfx::Effect> {
    let (name, ms) = fx.resolve(kind)?;
    // Skip pane chrome: transitions apply to text cells inside borders.
    let filter = CellFilter::AllOf(vec![
        CellFilter::Inner(Margin::new(1, 1)),
        CellFilter::Text,
    ]);
    let effect = match (kind, name.as_str()) {
        (_, "dissolve") => {
            fx::dissolve(EffectTimer::from_ms(ms, Interpolation::QuadOut)).with_filter(filter)
        }
        (FxKind::Close, "fade") => {
            fx::fade_to_fg(Color::Black, (ms, Interpolation::SineIn)).with_filter(filter)
        }
        (_, "fade") => {
            fx::fade_from_fg(Color::White, (ms, Interpolation::SineIn)).with_filter(filter)
        }
        (_, "flash") => {
            fx::fade_from_fg(Color::Yellow, (ms, Interpolation::SineIn)).with_filter(filter)
        }
        _ => return None,
    }
    .with_area(rect);
    Some(effect)
}

/// Recursively split area per tree, honouring each split's ratio.
fn collect_rects(node: &Node, area: Rect, out: &mut Vec<(usize, Rect)>) {
    match node {
        Node::Pane(id) => out.push((*id, area)),
        Node::Split {
            dir, ratio, first, second, ..
        } => {
            let direction = match dir {
                SplitDir::Horizontal => Direction::Vertical,
                SplitDir::Vertical => Direction::Horizontal,
            };
            let pct = (ratio.clamp(0.1, 0.9) * 100.0).round() as u16;
            let chunks = Layout::default()
                .direction(direction)
                .constraints([Constraint::Percentage(pct), Constraint::Percentage(100 - pct)])
                .split(area);
            collect_rects(first, chunks[0], out);
            collect_rects(second, chunks[1], out);
        }
    }
}

/// Top menu bar: just the `[Workspaces]`-style menu (no hints or tabs).
/// The menubar renders every frame; its dropdown opens on the menu key and
/// overlays the panes below because it renders after them.
fn render_menu_bar(frame: &mut Frame, app: &mut App, area: Rect) {
    // Keep the bar's workspace list current while the inbox is closed.
    app.sync_menu();
    let menu = app.menu_mut();
    Menu::new().render(area, frame.buffer_mut(), menu);
}

/// Command palette overlay (ctrl+p): a fuzzy query line over a filtered list.
fn render_palette(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = app.palette_ref() else {
        return;
    };
    let popup = centered(area, 60, 60);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" command palette (type to filter, enter run, esc close) ")
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);
    // Query line.
    let qblock = Block::default()
        .borders(Borders::ALL)
        .title(" query ")
        .border_style(Style::default().fg(Color::DarkGray));
    let qinner = qblock.inner(rows[0]);
    frame.render_widget(qblock, rows[0]);
    if qinner.width > 0 && qinner.height > 0 {
        let q = format!("{}▏", p.query);
        frame.render_widget(
            Paragraph::new(q).style(Style::default().fg(Color::White)),
            qinner,
        );
    }

    // Filtered list, scrolled to keep the selection visible.
    let view_h = rows[1].height as usize;
    if view_h == 0 {
        return;
    }
    let total = p.results.len();
    let start = if p.selected >= view_h {
        p.selected + 1 - view_h
    } else {
        0
    };
    let accent = app
        .theme_colors()
        .accent
        .map(|[r, g, b]| Color::Rgb(r, g, b))
        .unwrap_or(Color::Cyan);
    if total == 0 {
        frame.render_widget(
            Paragraph::new("  (no matching command)").style(Style::default().fg(Color::DarkGray)),
            rows[1],
        );
        return;
    }
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in p.results.iter().enumerate().skip(start).take(view_h) {
        let selected = i == p.selected;
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            if selected { "▶ " } else { "  " },
            Style::default().fg(accent),
        ));
        // Highlight matched characters.
        for (idx, ch) in item.label.chars().enumerate() {
            let hit = item.indices.contains(&idx);
            let style = if hit {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
}

/// Saved-command picker (ctrl+r): query line over the command database.
fn render_command_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let accent = app
        .theme_colors()
        .accent
        .map(|[r, g, b]| Color::Rgb(r, g, b))
        .unwrap_or(Color::Cyan);
    let Some(p) = app.command_picker_ref() else {
        return;
    };
    let popup = centered(area, 64, 64);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands (type to filter, enter run, ctrl+e edit, del remove, esc close) ")
        .border_style(Style::default().fg(accent));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(inner);
    let qblock = Block::default()
        .borders(Borders::ALL)
        .title(" query ")
        .border_style(Style::default().fg(Color::DarkGray));
    let qinner = qblock.inner(rows[0]);
    frame.render_widget(qblock, rows[0]);
    if qinner.width > 0 && qinner.height > 0 {
        frame.render_widget(
            Paragraph::new(format!("{}▏", p.query)).style(Style::default().fg(Color::White)),
            qinner,
        );
    }

    let view_h = rows[1].height as usize;
    if view_h == 0 {
        return;
    }
    if p.results.is_empty() {
        frame.render_widget(
            Paragraph::new("  (no saved commands match)")
                .style(Style::default().fg(Color::DarkGray)),
            rows[1],
        );
        return;
    }
    let start = if p.selected >= view_h {
        p.selected + 1 - view_h
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    for (i, hit) in p.results.iter().enumerate().skip(start).take(view_h) {
        let selected = i == p.selected;
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            if selected { "▶ " } else { "  " },
            Style::default().fg(accent),
        ));
        for (idx, ch) in hit.item.command.chars().enumerate() {
            let style = if hit.indices.contains(&idx) {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        if !hit.item.comment.trim().is_empty() {
            spans.push(Span::styled(
                format!("  {}", hit.item.comment.trim()),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut extra = String::new();
        if !hit.item.tags.trim().is_empty() {
            extra.push_str(&format!(" [{}]", hit.item.tags.trim()));
        }
        if hit.item.uses > 0 {
            extra.push_str(&format!(" ×{}", hit.item.uses));
        }
        if !extra.is_empty() {
            spans.push(Span::styled(extra, Style::default().fg(Color::DarkGray)));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
}

/// cheat.sh import review (tick rows with space, then Enter adds them).
fn render_cheat_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let accent = app
        .theme_colors()
        .accent
        .map(|[r, g, b]| Color::Rgb(r, g, b))
        .unwrap_or(Color::Cyan);
    let Some(p) = app.cheat_picker_ref() else {
        return;
    };
    let checked = p.checked_count();
    let total = p.items.len();
    let popup = centered(area, 72, 72);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(
            " import cheat.sh/{} ({checked}/{total} ticked, ctrl+e edit, enter add, esc cancel) ",
            p.topic
        ))
        .border_style(Style::default().fg(accent));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let qblock = Block::default()
        .borders(Borders::ALL)
        .title(" filter ")
        .border_style(Style::default().fg(Color::DarkGray));
    let qinner = qblock.inner(rows[0]);
    frame.render_widget(qblock, rows[0]);
    if qinner.width > 0 && qinner.height > 0 {
        frame.render_widget(
            Paragraph::new(format!("{}▏", p.query)).style(Style::default().fg(Color::White)),
            qinner,
        );
    }
    frame.render_widget(
        Paragraph::new(
            "space tick | ctrl+a all shown | ctrl+u clear | ctrl+e edit | enter add ticked | esc cancel",
        )
        .style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );

    let view_h = rows[1].height as usize;
    if view_h == 0 {
        return;
    }
    if p.results.is_empty() {
        frame.render_widget(
            Paragraph::new("  (no rows match)").style(Style::default().fg(Color::DarkGray)),
            rows[1],
        );
        return;
    }
    let start = if p.selected >= view_h {
        p.selected + 1 - view_h
    } else {
        0
    };
    let mut lines: Vec<Line> = Vec::new();
    for (i, hit) in p.results.iter().enumerate().skip(start).take(view_h) {
        let Some(item) = p.item(hit) else {
            continue;
        };
        let selected = i == p.selected;
        let mut spans: Vec<Span> = Vec::new();
        spans.push(Span::styled(
            if selected { "▶ " } else { "  " },
            Style::default().fg(accent),
        ));
        spans.push(Span::styled(
            if item.checked { "[x] " } else { "[ ] " },
            Style::default().fg(if item.checked {
                accent
            } else {
                Color::DarkGray
            }),
        ));
        for (idx, ch) in item.entry.command.chars().enumerate() {
            let style = if hit.indices.contains(&idx) {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(ch.to_string(), style));
        }
        if !item.entry.comment.trim().is_empty() {
            spans.push(Span::styled(
                format!("  {}", item.entry.comment.trim()),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[1]);
}

/// Font picker (palette "Change font..."): installed fonts, fuzzy filter,
/// Enter applies the highlighted one (updates `[general] font` live).
fn render_font_picker(frame: &mut Frame, app: &mut App, area: Rect) {
    let accent = app
        .theme_colors()
        .accent
        .map(|[r, g, b]| Color::Rgb(r, g, b))
            .unwrap_or(Color::Cyan);
    let preview_loading = app.font_preview_loading();
    // Spinner area captured while the picker is borrowed, rendered after
    // the borrow ends (`&mut app.throbber` would conflict with it).
    let mut spinner_area: Option<Rect> = None;
    {
        let Some(p) = app.font_picker_mut() else {
            return;
        };
        let popup = centered(area, 64, 64);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" fonts (tab: font/size, enter apply, esc close) ")
            .border_style(Style::default().fg(accent));
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if inner.width == 0 || inner.height < 6 {
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(8),
                Constraint::Min(1),
            ])
            .split(inner);
        // Query field.
        let q_focus = p.focus == crate::app::PickerFocus::List;
        let qblock = Block::default()
            .borders(Borders::ALL)
            .title(" query ")
            .border_style(Style::default().fg(if q_focus { accent } else { Color::DarkGray }));
        let qinner = qblock.inner(rows[0]);
        frame.render_widget(qblock, rows[0]);
        if qinner.width > 0 && qinner.height > 0 {
            frame.render_widget(
                Paragraph::new(format!("{}▏", p.query)).style(Style::default().fg(Color::White)),
                qinner,
            );
        }
        // Size field: focused shows the key hint and the pending digits.
        let s_focus = p.focus == crate::app::PickerFocus::Size;
        let sblock = Block::default()
            .borders(Borders::ALL)
            .title(if s_focus {
                " size (up/down or j/k, digits, tab back) "
            } else {
                " size (tab to edit) "
            })
            .border_style(Style::default().fg(if s_focus { accent } else { Color::DarkGray }));
        let sinner = sblock.inner(rows[1]);
        frame.render_widget(sblock, rows[1]);
        if sinner.width > 0 && sinner.height > 0 {
            let shown = p.size_display(s_focus);
            let style = if s_focus {
                Style::default().fg(accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            frame.render_widget(Paragraph::new(shown).style(style), sinner);
        }
        // Preview box: the GPU image pass draws the rasterized font here
        // (rendered off-thread; see `request_font_preview`). While a render
        // is in flight a spinner shows instead of a stale blank.
        frame.render_widget(Clear, rows[2]);
        let pblock = Block::default()
            .borders(Borders::ALL)
            .title(if preview_loading {
                " preview — loading… "
            } else {
                " preview "
            })
            .border_style(Style::default().fg(Color::DarkGray));
        let pinner = pblock.inner(rows[2]);
        frame.render_widget(pblock, rows[2]);
        if preview_loading && pinner.width > 0 && pinner.height > 0 {
            spinner_area = Some(Rect::new(
                pinner.x,
                pinner.y,
                pinner.width,
                pinner.height.min(1),
            ));
        }
        // List.
        let view_h = rows[3].height as usize;
        if view_h > 0 {
            if p.results.is_empty() {
                frame.render_widget(
                    Paragraph::new("  (no installed fonts match)")
                        .style(Style::default().fg(Color::DarkGray)),
                    rows[3],
                );
            } else {
                let start = if p.selected >= view_h {
                    p.selected + 1 - view_h
                } else {
                    0
                };
                let mut lines: Vec<Line> = Vec::new();
                // Lazy monospace flags for the visible rows (one parse per
                // font ever; unreadable stays untagged).
                p.probe_visible(start, view_h);
                for (i, hit) in p.results.iter().enumerate().skip(start).take(view_h) {
                    let selected = i == p.selected;
                    let mut spans: Vec<Span> = Vec::new();
                    spans.push(Span::styled(
                        if selected { "▶ " } else { "  " },
                        Style::default().fg(accent),
                    ));
                    let is_default = hit.item.path.as_os_str().is_empty();
                    let active = if is_default {
                        p.active.trim().is_empty()
                    } else {
                        hit.item.path.to_string_lossy() == p.active
                    };
                    spans.push(Span::styled(
                        if active { "● " } else { "○ " },
                        Style::default().fg(if active { accent } else { Color::DarkGray }),
                    ));
                    for (idx, ch) in hit.item.name.chars().enumerate() {
                        let style = if hit.indices.contains(&idx) {
                            Style::default().fg(accent).add_modifier(Modifier::BOLD)
                        } else if selected {
                            Style::default().fg(Color::White)
                        } else {
                            Style::default().fg(Color::Gray)
                        };
                        spans.push(Span::styled(ch.to_string(), style));
                    }
                    if hit.mono == Some(false) {
                        spans.push(Span::styled(
                            "  [not monospace]",
                            Style::default().fg(Color::Red),
                        ));
                    }
                    if !is_default {
                        spans.push(Span::styled(
                            format!("  {}", hit.item.path.display()),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    lines.push(Line::from(spans));
                }
                frame.render_widget(Paragraph::new(Text::from(lines)), rows[3]);
            }
        }
    }
    if let Some(area) = spinner_area {
        frame.render_stateful_widget(
            Throbber::default().label("loading"),
            area,
            &mut app.throbber,
        );
    }
}

/// Save-command form (ctrl+shift+r): command text + comment, Tab switches.
fn render_command_form(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(form) = app.command_form_mut() else {
        return;
    };
    let popup = centered(area, 64, 24);
    frame.render_widget(Clear, popup);
    let title = match form.edit_id {
        Some(id) => format!(" edit command #{id} (tab field, enter save, esc back) "),
        None => " save command (tab field, enter save, esc cancel) ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Green));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height < 4 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
    let focused = Style::default().fg(Color::Green);
    let idle = Style::default().fg(Color::DarkGray);
    frame.render_widget(
        Paragraph::new("command (use {name} for placeholders)")
            .style(if form.field == 0 { focused } else { idle }),
        rows[0],
    );
    frame.render_widget(&form.command, rows[1]);
    frame.render_widget(
        Paragraph::new("comment").style(if form.field == 1 { focused } else { idle }),
        rows[2],
    );
    frame.render_widget(&form.comment, rows[3]);
}

/// Placeholder fill-in form for a saved command.
fn render_command_args(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(args) = app.command_args_mut() else {
        return;
    };
    let popup = centered(area, 64, 40);
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" fill placeholders (enter/tab next, esc cancel) ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.width == 0 || inner.height < 2 {
        return;
    }
    let n = args.names.len() as u16;
    let mut constraints = Vec::new();
    constraints.push(Constraint::Length(2));
    for _ in 0..n {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Min(1));
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    frame.render_widget(
        Paragraph::new(format!(" {}", args.template))
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true }),
        rows[0],
    );
    for i in 0..args.names.len() {
        let title = format!(" {} ", args.names[i]);
        let color = if i == args.index {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        let b = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(color));
        let bi = b.inner(rows[1 + i]);
        frame.render_widget(b, rows[1 + i]);
        frame.render_widget(&args.fields[i], bi);
    }
}


fn render_selection(
    frame: &mut Frame,
    app: &App,
    rects: &[(usize, Rect)],
    focused: usize,
    theme: crate::config::ThemeColors,
) {
    let Some(sel) = app.select() else { return };
    let Some(rect) = rects.iter().find(|(id, _)| *id == focused).map(|(_, r)| *r) else {
        return;
    };
    let inner = Block::default().borders(Borders::ALL).inner(rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let accent = theme
        .accent
        .map(|[r, g, b]| Color::Rgb(r, g, b))
        .unwrap_or(Color::Yellow);
    let base_bg = Color::Rgb(theme.bg[0], theme.bg[1], theme.bg[2]);
    let buf = frame.buffer_mut();

    let mut mark = |r: u16, c: u16| {
        if r >= inner.height || c >= inner.width {
            return;
        }
        let cell = &mut buf[(inner.x + c, inner.y + r)];
        cell.set_bg(accent)
            .set_fg(base_bg)
            .set_style(Style::default().add_modifier(Modifier::BOLD));
    };

    match sel.range() {
        Some((r0, c0, r1, c1)) => {
            for r in r0..=r1 {
                let (start, end) = if r == r0 && r == r1 {
                    (c0, c1)
                } else if r == r0 {
                    (c0, inner.width.saturating_sub(1))
                } else if r == r1 {
                    (0, c1)
                } else {
                    (0, inner.width.saturating_sub(1))
                };
                for c in start..=end {
                    mark(r, c);
                }
            }
        }
        None => {
            let (r, c) = sel.cursor();
            mark(r, c);
        }
    }

    // The cursor end gets its own inverted style (accent text on terminal
    // background, bold + underlined) so it stands out from the rest of the
    // selection; the GPU bar/underline cursor is hidden while selecting.
    let (cr, cc) = sel.cursor();
    if cr < inner.height && cc < inner.width {
        let cell = &mut buf[(inner.x + cc, inner.y + cr)];
        cell.set_style(
            Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
        .set_fg(accent)
        .set_bg(base_bg);
    }
}

/// Transient bottom status line, only laid out while a message exists.
fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let bar = Paragraph::new(app.status.clone())
        .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(bar, area);
}

/// Textarea prompt below the panes (ratatui-textarea).
fn render_prompt(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = app.prompt_mut() else { return };
    let label = p.kind.label();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {label} (Enter=run, Tab=complete, Esc=cancel) "))
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(&p.editor, inner);

    // Ghost completion: dim text after the typed line.
    if !p.ghost.is_empty() && inner.width > 0 && inner.height > 0 {
        use unicode_width::UnicodeWidthStr;
        let typed = p.editor.lines().last().cloned().unwrap_or_default();
        let x = inner.x + (typed.width() as u16).min(inner.width.saturating_sub(1));
        let room = (inner.width - (x - inner.x)) as usize;
        if room > 0 {
            let ghost: String = p.ghost.chars().take(room).collect();
            frame
                .buffer_mut()
                .set_string(x, inner.y, &ghost, Style::default().fg(Color::DarkGray));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 200, 60)
    }

    /// Pixel aspect of the fitted inner rect, for assertions.
    fn aspect(rect: Rect, cell_w: f32, cell_h: f32) -> f32 {
        let inner = Block::default().borders(Borders::ALL).inner(rect);
        (inner.width as f32 * cell_w) / (inner.height as f32 * cell_h)
    }

    /// Inner (texture) cells of a popup rect.
    fn inner_of(rect: Rect) -> Rect {
        Block::default().borders(Borders::ALL).inner(rect)
    }

    /// A 16:9 image keeps ~16:9 in pixels and stays inside the max box.
    #[test]
    fn fitted_popup_keeps_wide_aspect() {
        let max = inner_of(image_popup(area()));
        let r = image_popup_fitted(area(), (1600, 900), 10.0, 20.0);
        let a = aspect(r, 10.0, 20.0);
        assert!(
            (a - 16.0 / 9.0).abs() < 0.08,
            "aspect {a} for {r:?}"
        );
        let inner = inner_of(r);
        assert!(inner.x >= max.x && inner.y >= max.y, "inner {inner:?}");
        assert!(inner.x + inner.width <= max.x + max.width);
        assert!(inner.y + inner.height <= max.y + max.height);
        // Width-constrained: fills the box width.
        assert_eq!(inner.width, max.width);
    }

    /// A tall image is height-constrained and centered, not stretched.
    #[test]
    fn fitted_popup_keeps_tall_aspect() {
        let max = inner_of(image_popup(area()));
        let r = image_popup_fitted(area(), (400, 1200), 10.0, 20.0);
        let a = aspect(r, 10.0, 20.0);
        assert!((a - (400.0 / 1200.0)).abs() < 0.08, "aspect {a}");
        let inner = inner_of(r);
        assert_eq!(inner.height, max.height);
        assert!(inner.width < max.width, "inner {inner:?}");
        // Roughly centered horizontally inside the max box.
        let left = inner.x - max.x;
        let right = (max.x + max.width) - (inner.x + inner.width);
        assert!((left as i32 - right as i32).abs() <= 1, "l={left} r={right}");
    }

    /// Degenerate inputs fall back to the plain max box.
    #[test]
    fn fitted_popup_falls_back_when_degenerate() {
        assert_eq!(image_popup_fitted(area(), (0, 0), 10.0, 20.0), image_popup(area()));
        assert_eq!(image_popup_fitted(area(), (100, 100), 0.0, 20.0), image_popup(area()));
        assert_eq!(image_popup_fitted(Rect::new(0, 0, 3, 3), (100, 100), 10.0, 20.0), image_popup(Rect::new(0, 0, 3, 3)));
    }

    fn test_theme() -> crate::config::ThemeColors {
        crate::config::ThemeColors {
            fg: [220, 220, 220],
            bg: [10, 10, 10],
            cursor: None,
            cursor_shape: crate::config::CursorShape::Block,
            accent: None,
        }
    }

    /// A full block becomes a space painted in the glyph color, so the
    /// backend draws a seamless background quad instead of a font glyph.
    #[test]
    fn full_block_becomes_solid_fill() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf[(0, 0)].set_symbol("█").set_fg(Color::Green).set_bg(Color::Black);
        buf[(1, 0)].set_symbol("▀").set_fg(Color::Green).set_bg(Color::Black);
        buf[(2, 0)].set_symbol("a").set_fg(Color::Green).set_bg(Color::Black);
        buf[(3, 0)].set_symbol(" ").set_fg(Color::Green).set_bg(Color::Black);
        normalize_block_cells(&mut buf, Rect::new(0, 0, 4, 2), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].fg, Color::Green);
        assert_eq!(buf[(0, 0)].bg, Color::Green);
        // Halves, text and spaces are untouched.
        assert_eq!(buf[(1, 0)].symbol(), "▀");
        assert_eq!(buf[(2, 0)].symbol(), "a");
        assert_eq!(buf[(3, 0)].symbol(), " ");
    }

    /// `REVERSED` is baked into the fill (the backend must not swap again).
    #[test]
    fn full_block_reversed_uses_bg() {
        use ratatui::buffer::Buffer;
        use ratatui::style::Modifier;
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf[(0, 0)]
            .set_symbol("█")
            .set_fg(Color::Red)
            .set_bg(Color::Blue);
        buf[(0, 0)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        normalize_block_cells(&mut buf, Rect::new(0, 0, 2, 1), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, Color::Blue);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// `Reset` resolves against the theme, like the backend does.
    #[test]
    fn full_block_reset_resolves_theme() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf[(0, 0)]
            .set_symbol("█")
            .set_fg(Color::Reset)
            .set_bg(Color::Reset);
        normalize_block_cells(&mut buf, Rect::new(0, 0, 2, 1), test_theme());
        assert_eq!(buf[(0, 0)].symbol(), " ");
        assert_eq!(buf[(0, 0)].bg, Color::Rgb(220, 220, 220));
    }

    /// The Unicode braille bit layout maps to the 2×4 dot grid.
    #[test]
    fn braille_bits_map_to_grid() {
        assert_eq!(braille_dot_cell(0), (0, 0));
        assert_eq!(braille_dot_cell(1), (0, 1));
        assert_eq!(braille_dot_cell(2), (0, 2));
        assert_eq!(braille_dot_cell(3), (1, 0));
        assert_eq!(braille_dot_cell(4), (1, 1));
        assert_eq!(braille_dot_cell(5), (1, 2));
        assert_eq!(braille_dot_cell(6), (0, 3));
        assert_eq!(braille_dot_cell(7), (1, 3));
    }

    /// A braille cell is blanked and becomes one quad per set dot, colored
    /// with the cell foreground; a blank braille and normal text are untouched.
    #[test]
    fn braille_cells_become_dots() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(0, 0)]
            .set_symbol("\u{28FF}")
            .set_fg(Color::Green)
            .set_bg(Color::Black);
        buf[(1, 0)]
            .set_symbol("\u{2800}")
            .set_fg(Color::White)
            .set_bg(Color::Black);
        buf[(2, 0)]
            .set_symbol("a")
            .set_fg(Color::White)
            .set_bg(Color::Black);
        let out: Arc<Mutex<Vec<Dot>>> = Arc::new(Mutex::new(Vec::new()));
        collect_braille(
            &mut buf,
            Rect::new(0, 0, 4, 1),
            Rect::new(0, 0, 4, 1),
            &out,
            test_theme().fg,
        );
        assert_eq!(buf[(0, 0)].symbol(), " ", "braille glyph blanked");
        assert_eq!(buf[(1, 0)].symbol(), " ", "blank braille blanked");
        assert_eq!(buf[(2, 0)].symbol(), "a", "text untouched");
        let dots = out.lock().unwrap();
        assert_eq!(dots.len(), 8, "U+28FF has all eight dots");
        for d in dots.iter() {
            assert!(d.ndc[0] < d.ndc[2], "positive width");
            assert!(d.ndc[1] > d.ndc[3], "positive height");
            assert_eq!(d.color, srgb_to_linear([0, 205, 0]));
        }
    }

    /// Outside the pane's inner rect nothing is collected.
    #[test]
    fn braille_ignores_outside_inner() {
        use ratatui::buffer::Buffer;
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
        buf[(3, 0)].set_symbol("\u{28FF}").set_fg(Color::Green);
        let out: Arc<Mutex<Vec<Dot>>> = Arc::new(Mutex::new(Vec::new()));
        collect_braille(
            &mut buf,
            Rect::new(0, 0, 3, 1),
            Rect::new(0, 0, 4, 1),
            &out,
            test_theme().fg,
        );
        assert_eq!(out.lock().unwrap().len(), 0);
        assert_eq!(buf[(3, 0)].symbol(), "\u{28FF}");
    }
}
