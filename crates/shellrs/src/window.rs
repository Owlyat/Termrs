//! Native window host: `winit` event loop + `ratatui-wgpu` backend.
//!
//! Shellrs renders itself into its own OS window (no external terminal).
//! Keyboard input arrives on the winit thread, is handed to a dedicated tokio
//! worker over a crossbeam channel for translation, and comes back as a user
//! event for the UI thread to apply. Each pane pumps its PTY on its own tokio
//! blocking thread (see `pty.rs`).

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, unbounded};
use ratatui::Terminal;
use ratatui::backend::Backend;
use ratatui_wgpu::{Builder, Dimensions, Font, WgpuBackend};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, KeyEvent as WinitKeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::app::App;
use crate::config::Config;
use crate::image_gpu::Dot;
use crate::image_gpu::ImagePostProcessor;
use crate::image_gpu::ProcessorUserData;
use crate::keys::{self, KeyPress, Mods};
use crate::ui;

use std::sync::Mutex;

/// Redraw cadence while panes are live (spinner + PTY output).
const FRAME: Duration = Duration::from_millis(16);

/// Backend lifetime pair + our image post-processor.
type Term = Terminal<WgpuBackend<'static, 'static, ImagePostProcessor>>;

/// Events produced by background threads: translated keys and PTY output.
#[derive(Debug)]
enum AppEvent {
    Key(KeyPress),
    /// A pane delivered output; poll and redraw immediately.
    Output,
}

/// Raw key data: winit thread hands these to the input worker untouched.
struct RawInput {
    event: WinitKeyEvent,
    mods: Mods,
}

/// Owns the window, GPU terminal, and app state.
pub struct ShellrsWindow {
    config: Config,
    layout_path: Option<std::path::PathBuf>,
    window: Option<Arc<Window>>,
    terminal: Option<Term>,
    app: Option<App>,
    mods: Mods,
    error: Option<String>,
    /// Boot, render one frame, then exit (used by `--selftest`).
    selftest: bool,
    /// Tokio handle for pane readers + the input worker.
    rt: tokio::runtime::Handle,
    /// Sends raw winit keys to the input worker.
    raw_tx: Option<Sender<RawInput>>,
    /// Loop proxy handed to the input worker so it can wake the UI thread.
    /// Set once in `run_inner`, taken in `init`.
    proxy: Option<EventLoopProxy<AppEvent>>,
    /// Loaded font bytes (primary + fallbacks) for backend rebuilds (zoom).
    font_set: Option<crate::font::FontSet>,
    /// Shared braille dot list (UI fills it; the post-processor draws it).
    braille: Arc<Mutex<Vec<Dot>>>,
    /// Last cursor position in physical pixels (for click hit-testing).
    cursor: PhysicalPosition<f64>,
    /// Left button currently held (for drag-motion mouse reports).
    left_held: bool,
    /// Window title currently set, to avoid redundant set_title calls.
    title: String,
    /// Path of the image currently uploaded to the GPU overlay.
    current_image: Option<String>,
    /// Cache key (path, px) of the font-picker preview on the GPU overlay.
    current_preview: Option<(String, u32)>,
    /// Backdrop key (path + opacity) currently uploaded.
    current_backdrop: Option<String>,
    /// HTTP server handle for remote control. Held (never read) so the server
    /// thread and its channels stay alive for the window's lifetime.
    #[allow(dead_code)]
    http_server: Option<crate::server::HttpServerHandle>,
    /// Address for the HTTP server (from --server flag).
    server_addr: Option<String>,
}

/// Monospace flag for one font path, for logging (`true`/`false`/`unknown`).
fn mono_flag(path: &str) -> String {
    match crate::font::is_monospaced(std::path::Path::new(path)) {
        Ok(mono) => mono.to_string(),
        Err(e) => format!("unknown ({e})"),
    }
}

impl ShellrsWindow {
    /// Prepare the handler; the window itself is created in `resumed`.
    /// `selftest` makes the first resume render one frame and exit.
    pub fn with_selftest(
        config: Config,
        layout_path: Option<std::path::PathBuf>,
        selftest: bool,
        rt: tokio::runtime::Handle,
    ) -> Self {
        Self {
            config,
            layout_path,
            window: None,
            terminal: None,
            app: None,
            mods: Mods::empty(),
            error: None,
            selftest,
            rt,
            raw_tx: None,
            proxy: None,
            font_set: None,
            braille: Arc::new(Mutex::new(Vec::new())),
            cursor: PhysicalPosition::new(0.0, 0.0),
            left_held: false,
            title: String::new(),
            current_image: None,
            current_preview: None,
            current_backdrop: None,
            http_server: None,
            server_addr: None,
        }
    }

    /// Build the GPU terminal for `window` at the given glyph height.
    fn build_backend(&self, window: &Arc<Window>, font_size: u32) -> Result<Term, String> {
        let set = self.font_set.as_ref().expect("font loaded in init");
        let font = Font::new(set.primary).ok_or("font could not be parsed")?;
        let fallbacks: Vec<Font> = set
            .fallbacks
            .iter()
            .filter_map(|b| Font::new(b))
            .collect();

        let size = window.inner_size();
        let width = NonZeroU32::new(size.width.max(1)).expect("non-zero width");
        let height = NonZeroU32::new(size.height.max(1)).expect("non-zero height");

        let theme = self.config.theme.resolve();
        let clear = [
            theme.bg[0] as f64,
            theme.bg[1] as f64,
            theme.bg[2] as f64,
            1.0,
        ];
        // The primary must ALSO head the regular list: `from_font*` alone
        // leaves it as last-resort (only glyphs nothing else covers), so the
        // fallback would render all normal text and picks looked like no-ops.
        // Forcing it into `regular` (regardless of its italic/bold flags)
        // makes the picked font actually render; `select_font` stops at the
        // first full-coverage candidate, i.e. this one for normal text.
        let backend = pollster::block_on(
            Builder::from_font_and_user_data(
                font.clone(),
                ProcessorUserData {
                    braille: self.braille.clone(),
                    clear,
                    bg: theme.bg,
                },
            )
                .with_regular_fonts([font])
                .with_fonts(fallbacks)
                .with_width_and_height(Dimensions { width, height })
                .with_font_size_px(font_size.max(6))
                .with_fg_color(ratatui::style::Color::Rgb(
                    theme.fg[0],
                    theme.fg[1],
                    theme.fg[2],
                ))
                .with_bg_color(ratatui::style::Color::Rgb(
                    theme.bg[0],
                    theme.bg[1],
                    theme.bg[2],
                ))
                .build_with_target(window.clone()),
        )
        .map_err(|e| format!("wgpu backend: {e}"))?;
        Terminal::new(backend).map_err(|e| format!("terminal: {e}"))
    }

    /// Log every loaded font with its monospace flag, so the backend's
    /// generic "Non monospace font" warning can be attributed to a specific
    /// file (it fires per font, including fallbacks).
    fn log_font_set(&self, configured: &str, fallbacks: &[String]) {
        if configured.trim().is_empty() {
            log::info!("font primary: auto-detect");
        } else {
            log::info!(
                "font primary {configured:?} (monospace: {})",
                mono_flag(configured)
            );
        }
        let resolved = crate::font::fallback_paths(fallbacks);
        if resolved.is_empty() {
            log::info!("font fallbacks: none");
        } else {
            for (i, p) in resolved.iter().enumerate() {
                let shown = p.to_string_lossy().into_owned();
                log::info!("font fallback[{i}] {} (monospace: {})", p.display(), mono_flag(&shown));
            }
        }
    }

    /// Rebuild the GPU backend at a new glyph height (font zoom).
    /// App state (panes, layout) is untouched; the next draw refills.
    /// The live backend is dropped first: a second GPU init while it is
    /// alive never returns.
    fn apply_font_size(&mut self, size: u32) {
        let Some(window) = self.window.clone() else {
            return;
        };
        self.terminal = None;
        match self.build_backend(&window, size) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
                // Layers live in the processor, which was just recreated.
                self.current_backdrop = None;
                self.current_image = None;
                self.current_preview = None;
                log::info!("font size {size}px");
                window.request_redraw();
            }
            Err(e) => {
                self.error = Some(format!("font size {size}px: {e}"));
            }
        }
    }

    /// Rebuild the GPU backend with a new primary font file (font picker).
    /// Empty `path` restores auto-detection. The primary is parse-checked
    /// before the live backend is touched; a GPU build failure then tries to
    /// restore the previous font, and only exits when that fails too. The
    /// live backend is always dropped first: a second GPU init while it is
    /// alive never returns.
    fn apply_font(&mut self, path: &str, size: u32, fallbacks: &[String]) {
        let set = match crate::font::load_set(path, fallbacks) {
            Ok(set) => set,
            Err(e) => {
                log::warn!("font change failed: {e}");
                if let Some(app) = self.app.as_mut() {
                    app.status = format!("font change failed: {e}");
                }
                return;
            }
        };
        if ratatui_wgpu::Font::new(set.primary).is_none() {
            log::warn!("font change failed: cannot parse {path:?}");
            if let Some(app) = self.app.as_mut() {
                app.status = format!("font rejected ({path:?}): cannot parse");
            }
            return;
        }
        self.log_font_set(path, fallbacks);
        let shown = if path.trim().is_empty() {
            "system default".to_string()
        } else {
            path.to_string()
        };
        let Some(window) = self.window.clone() else {
            // No window yet: remember the bytes for the first backend build.
            self.font_set = Some(set);
            return;
        };
        let prev = self.font_set.replace(set);
        self.terminal = None;
        match self.build_backend(&window, size) {
            Ok(terminal) => {
                self.terminal = Some(terminal);
                self.current_backdrop = None;
                self.current_image = None;
                self.current_preview = None;
                log::info!("font {shown} ({size}px)");
                window.request_redraw();
            }
            Err(e) => {
                // Old backend is gone: try to restore the previous font.
                self.font_set = prev;
                match self.build_backend(&window, size) {
                    Ok(terminal) => {
                        self.terminal = Some(terminal);
                        self.current_backdrop = None;
                        self.current_image = None;
                        self.current_preview = None;
                        log::warn!("font {shown} rejected ({e}); restored previous font");
                        if let Some(app) = self.app.as_mut() {
                            app.status = format!("font rejected ({shown}): {e}");
                        }
                        window.request_redraw();
                    }
                    Err(e2) => {
                        self.error = Some(format!(
                            "font {shown}: {e}; rollback also failed: {e2}"
                        ));
                    }
                }
            }
        }
    }

    /// Create window, font, GPU backend, workers and app state on first
    /// resume.
    fn init(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        if self.window.is_some() {
            return Ok(());
        }
        let attrs = Window::default_attributes()
            .with_title("shellrs")
            .with_inner_size(LogicalSize::new(1280.0, 800.0))
            .with_decorations(self.config.window.decorations)
            .with_transparent(self.config.window.transparent);
        let window = Arc::new(
            event_loop
                .create_window(attrs)
                .map_err(|e| format!("create window: {e}"))?,
        );
        log::info!(
            "window {}x{} decorations={}",
            window.inner_size().width,
            window.inner_size().height,
            self.config.window.decorations
        );
        if self.config.window.transparent && cfg!(windows) {
            log::warn!(
                "transparent window requested, but per-pixel transparency is not \
                 supported by the GPU backend on Windows (swapchain is always \
                 opaque); showing an opaque terminal instead"
            );
        }

        // Headless font check: boot the backend with this file instead of
        // the configured one (renders frames, prints ok/fail, exits).
        if let Ok(test_font) = std::env::var("SHELLRS_SELFTEST_FONT") {
            log::info!("selftest font override: {test_font:?}");
            self.config.general.font = test_font;
        }

        let size = self.config.general.font_size.max(6);
        let custom = self.config.general.font.trim().to_string();
        let fallbacks = self.config.general.font_fallback.clone();
        // A bad custom font must not brick startup: retry with auto-detect.
        let font_set = match crate::font::load_set(&custom, &fallbacks) {
            Ok(set) => set,
            Err(e) if !custom.is_empty() => {
                log::warn!("font {custom:?} failed to load ({e}); falling back to auto-detect");
                crate::font::load_set("", &fallbacks)?
            }
            Err(e) => return Err(format!("font load: {e}")),
        };
        self.log_font_set(&custom, &fallbacks);
        log::info!(
            "fonts: 1 primary + {} fallback(s), {size}px",
            font_set.fallbacks.len(),
        );
        self.font_set = Some(font_set);
        let mut terminal = match self.build_backend(&window, size) {
            Ok(t) => t,
            Err(e) if !custom.is_empty() => {
                log::warn!("backend rejected font {custom:?} ({e}); falling back to auto-detect");
                let set = crate::font::load_set("", &fallbacks)?;
                self.log_font_set("", &fallbacks);
                self.font_set = Some(set);
                self.build_backend(&window, size)?
            }
            Err(e) => return Err(e),
        };

        // Output watcher: pane readers nudge this channel per chunk; each
        // nudge becomes an immediate poll + redraw instead of waiting for
        // the 16ms heartbeat.
        let (act_tx, act_rx) = unbounded::<()>();
        let proxy = self.proxy.take().expect("proxy set before run");
        let proxy_out = proxy.clone();
        self.rt.spawn_blocking(move || {
            while act_rx.recv().is_ok() {
                if proxy_out.send_event(AppEvent::Output).is_err() {
                    break;
                }
            }
        });

        let ipc = crate::ipc::start_server();
        let http_server = self.server_addr.clone().and_then(|addr| {
            match crate::server::start_server(&addr) {
                Ok(h) => Some(h),
                Err(e) => {
                    log::warn!("http server failed to start: {e}");
                    None
                }
            }
        });
        let server_mode = self.server_addr.is_some();
        if server_mode
            && let Ok(cwd) = std::env::current_dir() {
                log::info!("server mode: using working directory {}", cwd.display());
                let _ = std::env::set_current_dir(&cwd);
            }
        let mut app = App::new(
            self.config.clone(),
            self.layout_path.clone(),
            &self.rt,
            &act_tx,
            Some(ipc),
            http_server,
            server_mode,
        )?;
        // Share one braille dot buffer between the UI (writer) and the GPU
        // post-processor (reader).
        app.braille = self.braille.clone();
        log::info!("app booted");

        // Input worker: translate raw keys off the UI thread, then wake the
        // loop with the translated press as a user event.
        let (raw_tx, raw_rx) = unbounded::<RawInput>();
        self.rt.spawn_blocking(move || {
            while let Ok(raw) = raw_rx.recv() {
                if let Some(press) = keys::from_winit(&raw.event, raw.mods) {
                    // Translated result, right before dispatch: the pair with
                    // the raw log above pinpoints any binding mismatch.
                    log::debug!(
                        "press key={:?} mods={:?}",
                        press.key,
                        press.mods
                    );
                    if proxy.send_event(AppEvent::Key(press)).is_err() {
                        break;
                    }
                }
            }
        });
        self.raw_tx = Some(raw_tx);

        // First draw through the fresh terminal to catch backend errors early.
        terminal
            .draw(|frame| ui::draw(frame, &mut app))
            .map(|_| ())
            .map_err(|e| format!("first draw: {e}"))?;

        self.terminal = Some(terminal);
        self.app = Some(app);
        self.window = Some(window);
        Ok(())
    }

    /// Map the last cursor pixel position to a terminal cell, using the
    /// backend's character metrics.
    fn cursor_cell(&mut self) -> Option<(u16, u16)> {
        let terminal = self.terminal.as_mut()?;
        let ws = terminal
            .backend_mut()
            .window_size()
            .ok()?;
        if ws.columns_rows.width == 0 || ws.columns_rows.height == 0 {
            return None;
        }
        let cell_w = ws.pixels.width as f64 / ws.columns_rows.width as f64;
        let cell_h = ws.pixels.height as f64 / ws.columns_rows.height as f64;
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return None;
        }
        let col = (self.cursor.x / cell_w) as u16;
        let row = (self.cursor.y / cell_h) as u16;
        Some((col, row))
    }

    /// Render one frame.
    fn redraw(&mut self) -> Result<(), String> {
        // Push/refresh the backdrop and overlay before drawing (the GPU pass
        // runs during draw's flush).
        self.sync_backdrop();
        if !self.sync_font_preview() {
            self.sync_image();
        }
        // Cell pixels must reach the panes before `draw` resizes them, so
        // `CSI 16 t` replies and PTY pixel dimensions stay truthful.
        self.sync_cell_px();
        let (Some(terminal), Some(app)) = (self.terminal.as_mut(), self.app.as_mut()) else {
            return Ok(());
        };
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map(|_| ())
            .map_err(|e| format!("draw: {e}"))?;
        // Reflect the focused pane in the OS window title.
        if let Some(w) = &self.window {
            let title = app.window_title();
            if title != self.title {
                self.title = title.clone();
                w.set_title(&title);
            }
        }
        self.sync_cursor();
        Ok(())
    }

    /// Draw the terminal cursor (bar/underline) as a thin GPU quad. Runs after
    /// `draw` so the pane rects are up to date.
    fn sync_cursor(&mut self) {
        use crate::config::CursorShape;
        let theme = self.app.as_ref().map(|a| a.theme_colors());
        let cell = self.app.as_ref().and_then(|a| a.cursor_cell());
        let Some((_cells, cell_w, cell_h)) = self.cell_metrics() else {
            return;
        };
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let processor = terminal.backend_mut().post_processor_mut();
        let Some(theme) = theme else {
            processor.clear_cursor();
            return;
        };
        // A block cursor is painted into the cell buffer by the ui.
        if theme.cursor_shape == CursorShape::Block {
            processor.clear_cursor();
            return;
        }
        let Some((inner, row, col)) = cell else {
            processor.clear_cursor();
            return;
        };
        let (cw, ch) = (cell_w.max(1.0), cell_h.max(1.0));
        let thick = ((cw / 8.0).round()).max(2.0);
        let x = inner.x as f32 * cw + col as f32 * cw;
        let y = inner.y as f32 * ch + row as f32 * ch;
        let rect = match theme.cursor_shape {
            CursorShape::Underline => (x, y + ch - thick, cw, thick),
            CursorShape::Block => (x, y, cw, ch),
            CursorShape::Bar => (x, y, thick, ch),
        };
        let rgb = theme.cursor.unwrap_or(theme.fg);
        processor.set_cursor(rect, rgb);
    }

    /// Upload/refresh the background image layer (opacity applies only here).
    fn sync_backdrop(&mut self) {
        let path = self.config.window.background_image.clone();
        let opacity = self.config.window.clamped_opacity();
        let path_key = path.clone();
        if self.current_backdrop.as_deref() == Some(path_key.as_str()) {
            // Same image: only the alpha may have changed (no re-decode).
            if let Some(terminal) = self.terminal.as_mut() {
                terminal
                    .backend_mut()
                    .post_processor_mut()
                    .set_backdrop_opacity(opacity);
            }
            return;
        }
        let Some(terminal) = self.terminal.as_mut() else {
            return;
        };
        let processor = terminal.backend_mut().post_processor_mut();
        if path.trim().is_empty() {
            processor.set_backdrop(Vec::new(), 0, 0, 0.0);
        } else {
            match crate::image_view::decode(&path) {
                Ok(img) => {
                    log::info!("backdrop {} ({}x{}) opacity {opacity}", path, img.width, img.height);
                    processor.set_backdrop(img.data, img.width, img.height, opacity);
                }
                Err(e) => {
                    eprintln!("shellrs: background image {e}");
                    processor.set_backdrop(Vec::new(), 0, 0, 0.0);
                }
            }
        }
        self.current_backdrop = Some(path_key);
    }

    /// Upload/position the font-picker preview on the GPU overlay. Returns
    /// true when a preview owns the overlay this frame (so the ctrl+i image
    /// pass is skipped). Pixels arrive asynchronously from the preview
    /// worker; retained copies re-upload cleanly after backend rebuilds.
    fn sync_font_preview(&mut self) -> bool {
        let meta = self.app.as_ref().and_then(|a| a.font_preview_meta());
        let Some(((path, px), w, h)) = meta else {
            if self.current_preview.is_some() {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.backend_mut().post_processor_mut().clear_image();
                }
                self.current_preview = None;
                // Let the image viewer re-upload if it is also open.
                self.current_image = None;
            }
            return false;
        };
        let Some((cells, cell_w, cell_h)) = self.cell_metrics() else {
            return true;
        };
        let inner = crate::ui::font_preview_inner(cells);
        let box_w = inner.width as f32 * cell_w;
        let box_h = inner.height as f32 * cell_h;
        // Show the raster 1:1 so the chosen size is visible; shrink only when
        // the box is too small (never upscale -- that just blurs glyphs).
        let scale = (box_w / w.max(1) as f32)
            .min(box_h / h.max(1) as f32)
            .min(1.0);
        let dw = w as f32 * scale;
        let dh = h as f32 * scale;
        let margin = (cell_w * 0.5).max(1.0);
        let x = inner.x as f32 * cell_w + margin;
        let y = inner.y as f32 * cell_h + ((box_h - dh) / 2.0).max(0.0);
        let rect = (x, y, dw.max(1.0), dh.max(1.0));
        let key = (path, px);
        let Some(terminal) = self.terminal.as_mut() else {
            return true;
        };
        let processor = terminal.backend_mut().post_processor_mut();
        if self.current_preview.as_ref() == Some(&key) {
            processor.set_rect(rect);
        } else {
            let rgba = self
                .app
                .as_ref()
                .and_then(|a| a.font_preview_pixels())
                .unwrap_or_default();
            if rgba.is_empty() {
                // Pixels not ready yet (worker still rendering): keep showing
                // whatever is there until they land.
                return true;
            }
            processor.set_image(rgba, w, h, rect);
            self.current_preview = Some(key);
            self.current_image = None;
        }
        true
    }

    /// Compute and upload/position the GPU image overlay for the current
    /// image, using the same popup rect the ui draws its border at.
    fn sync_image(&mut self) {
        // Read what we need from the app first (short borrows), then touch the
        // terminal, to keep the borrow checker happy and avoid a clone.
        let meta = self
            .app
            .as_ref()
            .and_then(|a| a.image_ref().map(|i| (i.path.clone(), i.dims)));
        let Some((path, dims)) = meta else {
            if self.current_image.is_some() {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.backend_mut().post_processor_mut().clear_image();
                }
                self.current_image = None;
            }
            if let Some(app) = self.app.as_mut() {
                app.set_image_rect(None);
            }
            return;
        };

        // Cell metrics + aspect-fitted popup rect -> physical pixel rect for
        // the inner area. The fitted rect is stashed for the ui border so
        // chrome and texture share one rect (no stretching).
        let Some((cells, cell_w, cell_h)) = self.cell_metrics() else {
            return;
        };
        let popup = crate::ui::image_popup_fitted(cells, dims, cell_w, cell_h);
        if let Some(app) = self.app.as_mut() {
            app.set_image_rect(Some(popup));
        }
        let inner = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .inner(popup);
        let rect = (
            inner.x as f32 * cell_w,
            inner.y as f32 * cell_h,
            inner.width as f32 * cell_w,
            inner.height as f32 * cell_h,
        );

        if self.current_image.as_deref() == Some(path.as_str()) {
            if let Some(terminal) = self.terminal.as_mut() {
                terminal.backend_mut().post_processor_mut().set_rect(rect);
            }
        } else {
            // Move the pixels out (no multi-MB copy).
            let rgba = self
                .app
                .as_mut()
                .and_then(|a| a.take_image_rgba())
                .unwrap_or_default();
            if let Some(terminal) = self.terminal.as_mut() {
                terminal
                    .backend_mut()
                    .post_processor_mut()
                    .set_image(rgba, dims.0, dims.1, rect);
            }
            self.current_image = Some(path);
        }
    }

    /// Push the GPU cell size into every pane so `CSI 16 t` replies and the
    /// `TIOCGWINSZ` pixel dimensions on PTY resize report truthful values.
    /// Without this, graphics probes (e.g. `ratatui-image`) fall back to a
    /// guessed 10x20 cell and halfblock art renders with a wrong aspect.
    fn sync_cell_px(&mut self) {
        let Some((_cells, cell_w, cell_h)) = self.cell_metrics() else {
            return;
        };
        let (cw, ch) = (
            cell_w.round().max(1.0) as u32,
            cell_h.round().max(1.0) as u32,
        );
        let Some(app) = self.app.as_mut() else {
            return;
        };
        for id in app.ws().leaf_ids() {
            if let Some(p) = app.ws_mut().pane_mut(id) {
                p.set_cell_px(cw, ch);
            }
        }
    }

    /// Cell size and grid dimensions from the backend.
    fn cell_metrics(&mut self) -> Option<(ratatui::layout::Rect, f32, f32)> {
        let terminal = self.terminal.as_mut()?;
        let ws = terminal.backend_mut().window_size().ok()?;
        if ws.columns_rows.width == 0 || ws.columns_rows.height == 0 {
            return None;
        }
        let cw = ws.pixels.width as f32 / ws.columns_rows.width as f32;
        let ch = ws.pixels.height as f32 / ws.columns_rows.height as f32;
        Some((
            ratatui::layout::Rect::new(0, 0, ws.columns_rows.width, ws.columns_rows.height),
            cw,
            ch,
        ))
    }
}

impl ApplicationHandler<AppEvent> for ShellrsWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(e) = self.init(event_loop) {
            log::error!("init failed: {e}");
            self.error = Some(e);
            event_loop.exit();
            return;
        }
        if self.selftest {
            // Optional: validate the GPU image pass against a real file.
            if let Ok(path) = std::env::var("SHELLRS_SELFTEST_IMAGE")
                && let Some(app) = self.app.as_mut() {
                    match crate::image_view::ImageView::open(&path) {
                        Ok(img) => {
                            println!(
                                "shellrs: selftest image {} ({}x{})",
                                path, img.dims.0, img.dims.1
                            );
                            app.set_image_for_test(img);
                        }
                        Err(e) => eprintln!("shellrs: selftest image failed: {e}"),
                    }
                }
            // Optional: exercise a live backend rebuild (zoom, then font) to
            // diagnose rebuild hangs/crashes without clicking through the UI.
            if std::env::var("SHELLRS_SELFTEST_REBUILD").is_ok() {
                let size = self.config.general.font_size.max(6);
                eprintln!("shellrs: selftest rebuild: zoom pass...");
                self.apply_font_size(size);
                eprintln!("shellrs: selftest rebuild: zoom pass done");
                let fb = self.config.general.font_fallback.clone();
                let font = self.config.general.font.clone();
                eprintln!("shellrs: selftest rebuild: font pass...");
                self.apply_font(&font, size, &fb);
                eprintln!("shellrs: selftest rebuild: font pass done");
            }
            let outcome = self
                .terminal
                .as_mut()
                .map(|t| t.size())
                .transpose()
                .map_err(|e| format!("size: {e}"))
                .and_then(|size| {
                    let size = size.ok_or("no terminal size")?;
                    // Two frames: the first uploads the image, the second draws
                    // it through the post-process pass.
                    self.redraw()?;
                    self.redraw()?;
                    Ok(size)
                });
            match outcome {
                Ok(size) => println!(
                    "shellrs: selftest ok ({}x{} cells, {} panes, font {}px{})",
                    size.width,
                    size.height,
                    self.app.as_ref().map(|a| a.ws().leaf_ids().len()).unwrap_or(0),
                    self.config.general.font_size.max(6),
                    if self.config.general.font.trim().is_empty() {
                        String::from(", auto font")
                    } else {
                        format!(", font {:?}", self.config.general.font)
                    },
                ),
                Err(e) => {
                    log::error!("selftest failed: {e}");
                    self.error = Some(e);
                }
            }
            event_loop.exit();
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                // Forward motion while a tracked app wants it; cheap no-op
                // otherwise (no redraw: motion alone changes nothing visual).
                if let Some((col, row)) = self.cursor_cell()
                    && let Some(app) = self.app.as_mut() {
                        app.mouse_move(col, row, self.mods, self.left_held);
                    }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                if button == MouseButton::Left {
                    self.left_held = pressed;
                }
                let Some(mbutton) = (match button {
                    MouseButton::Left => Some(crate::mouse::MouseButton::Left),
                    MouseButton::Middle => Some(crate::mouse::MouseButton::Middle),
                    MouseButton::Right => Some(crate::mouse::MouseButton::Right),
                    _ => None,
                }) else {
                    return;
                };
                if let Some((col, row)) = self.cursor_cell() {
                    if let Some(app) = self.app.as_mut() {
                        // Menu bar / dropdown first: a `Menu` outcome means
                        // the click was consumed by the UI (opened, switched,
                        // activated, or dismissed the menu) — skip focus and
                        // never forward bytes for it. Otherwise legacy
                        // click-to-focus stays exactly as before, then the
                        // click goes to the pane (tracking apps like osutty
                        // get press/release at the cursor cell).
                        let outcome = app.mouse_button(col, row, mbutton, pressed, self.mods);
                        if outcome != crate::app::MouseClickOutcome::Menu
                            && pressed
                            && mbutton == crate::mouse::MouseButton::Left
                        {
                            app.focus_pane_at(col, row);
                        }
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // Up = older lines (positive delta -> scroll up).
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as i32 * 3,
                    MouseScrollDelta::PixelDelta(p) => (p.y / 12.0) as i32,
                };
                if lines != 0 {
                    // Position-aware: a tracking pane under the cursor
                    // gets wheel buttons, else legacy pane scrollback.
                    let cell = self.cursor_cell();
                    if let Some(app) = self.app.as_mut() {
                        match cell {
                            Some((col, row)) => {
                                app.mouse_wheel(col, row, lines, self.mods);
                            }
                            None => app.scroll_focused(lines),
                        }
                    }
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(size) => {
                if let Some(terminal) = self.terminal.as_mut() {
                    terminal.backend_mut().resize(size.width, size.height);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                self.mods = mods_from(m.state());
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // Presses, auto-repeats *and* releases go to the input worker;
                // the translated event comes back as a user event. Releases are
                // only observable by a child in `win32-input-mode`, but they
                // must reach it for held keys (sliders) to work.
                if matches!(event.state, ElementState::Pressed | ElementState::Released)
                    && let Some(tx) = &self.raw_tx
                {
                    // Debug aid: `--debug` reveals exactly what the platform
                    // reports (letter vs C0 control char).
                    log::debug!(
                        "key logical={:?} text={:?} state={:?} mods={:?}",
                        event.logical_key,
                        event.text,
                        event.state,
                        self.mods
                    );
                    let _ = tx.send(RawInput {
                        event: event.clone(),
                        mods: self.mods,
                    });
                }
            }
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.redraw() {
                    log::error!("redraw failed: {e}");
                    self.error = Some(e);
                    event_loop.exit();
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Key(press) => {
                // Drain requests first (short borrow), then rebuild: the
                // rebuilds borrow `self` mutably and must not overlap it.
                let (zoom, font_req) = match self.app.as_mut() {
                    Some(app) => {
                        app.handle_key(press);
                        let zoom = app.take_zoom_request();
                        let font = app.take_font_request().map(|p| {
                            (
                                p,
                                app.font_size(),
                                app.config.general.font_fallback.clone(),
                            )
                        });
                        (zoom, font)
                    }
                    None => (None, None),
                };
                if let Some(size) = zoom {
                    self.apply_font_size(size);
                }
                if let Some((path, size, fallbacks)) = font_req {
                    self.apply_font(&path, size, &fallbacks);
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            AppEvent::Output => {
                if let Some(app) = self.app.as_mut() {
                    app.poll_panes();
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
        }
        if self.error.is_some() {
            event_loop.exit();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_mut() {
            app.poll_panes();
            if app.should_quit {
                event_loop.exit();
                return;
            }
        }
        // Hot reload replaces `app.config` in place; mirror it into the
        // window host's copy so `[window]` (backdrop, decorations, theme for
        // rebuilds) follows live edits instead of going stale.
        let stale = self
            .app
            .as_ref()
            .is_some_and(|app| app.config != self.config);
        if stale {
            let fresh = self
                .app
                .as_ref()
                .map(|app| app.config.clone())
                .expect("app present");
            if fresh.window.decorations != self.config.window.decorations
                && let Some(w) = &self.window {
                    w.set_decorations(fresh.window.decorations);
                }
            if fresh.window.transparent != self.config.window.transparent {
                log::warn!("transparent changed; restart shellrs to apply");
                if let Some(app) = self.app.as_mut() {
                    app.status = "transparent changed: restart to apply".into();
                }
            }
            self.config = fresh;
        }
        // Hot reload may have queued a font change without any keypress.
        let font_req = self.app.as_mut().and_then(|app| {
            app.take_font_request().map(|p| {
                (
                    p,
                    app.font_size(),
                    app.config.general.font_fallback.clone(),
                )
            })
        });
        if let Some((path, size, fallbacks)) = font_req {
            self.apply_font(&path, size, &fallbacks);
        }
        if self.error.is_some() {
            event_loop.exit();
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }
}

/// Translate winit modifier state into our own flags.
fn mods_from(m: ModifiersState) -> Mods {
    let mut out = Mods::empty();
    if m.control_key() {
        out.insert(Mods::CONTROL);
    }
    if m.alt_key() {
        out.insert(Mods::ALT);
    }
    if m.shift_key() {
        out.insert(Mods::SHIFT);
    }
    if m.super_key() {
        out.insert(Mods::SUPER);
    }
    out
}

/// Run the native window until it closes. Returns the first fatal error, if any.
pub fn run(config: Config, layout_path: Option<std::path::PathBuf>, server_addr: Option<&str>) -> Result<(), String> {
    run_inner(config, layout_path, false, server_addr)
}

/// Boot the window, render one frame, then exit. For smoke tests / CI.
pub fn run_selftest(config: Config) -> Result<(), String> {
    run_inner(config, None, true, None)
}

/// Shared event-loop driver for normal and self-test runs.
///
/// A multi-thread tokio runtime lives for the whole run: pane readers and the
/// input worker are `spawn_blocking` tasks on it, while winit owns the main
/// thread. Dropping it at the end aborts the workers as the process exits.
fn run_inner(
    config: Config,
    layout_path: Option<std::path::PathBuf>,
    selftest: bool,
    server_addr: Option<&str>,
) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("shellrs-worker")
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let event_loop = EventLoop::<AppEvent>::with_user_event()
        .build()
        .map_err(|e| format!("event loop: {e}"))?;
    let mut handler =
        ShellrsWindow::with_selftest(config, layout_path, selftest, rt.handle().clone());
    handler.server_addr = server_addr.map(|s| s.to_string());
    handler.proxy = Some(event_loop.create_proxy());
    event_loop
        .run_app(&mut handler)
        .map_err(|e| format!("run: {e}"))?;
    // Detach without waiting: pane readers block in PTY reads until their
    // shells exit, and dropping the runtime would stall on them instead of
    // letting the process quit. The OS reclaims the threads on exit.
    rt.shutdown_background();
    match handler.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}
