//! One emulated terminal pane: `portable-pty` child + `vt100` screen.
//!
//! Each pane owns one tokio blocking task that pumps the PTY master into a
//! bounded crossbeam channel; the UI thread drains it in [`Pane::poll`]. The
//! CPU-heavy parts (ANSI stripping for the yank buffer) run on that reader
//! task, so panes strip in parallel and the UI thread stays responsive.

use std::io::{Read, Write};
use std::time::Duration;

use crossbeam_channel::{Receiver, bounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Default size before first layout assigns real rows/cols.
const FALLBACK_ROWS: u16 = 24;
const FALLBACK_COLS: u16 = 80;

/// One chunk of PTY output: raw bytes for the vt100 parser, plus the text
/// already stripped of escape sequences (with prompt-marker offsets) for the
/// yank buffer, plus any OSC 7 directory reports for cwd tracking.
/// Stripping happens on the pane's reader task.
pub struct Chunk {
    raw: Vec<u8>,
    text: String,
    marks: Vec<usize>,
    osc7: Vec<std::path::PathBuf>,
}

/// Single terminal pane.
pub struct Pane {
    pub id: usize,
    pub title: String,
    parser: vt100::Parser,
    rx: Receiver<Chunk>,
    writer: Box<dyn Write + Send>,
    #[allow(dead_code)]
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    /// Last size pushed to PTY; skip redundant resizes.
    size: (u16, u16),
    /// True once child exited; pane keeps scrollback visible.
    pub dead: bool,
    /// Tail of the previous chunk, so a query split across reads (e.g. a
    /// trailing `ESC[1` + `6t` head) is still answered.
    query_tail: Vec<u8>,
    /// Character cell size in pixels `(width, height)`, pushed from the GPU
    /// backend (`window.rs`). Used for `CSI 16 t` replies and for reporting
    /// real window pixels on PTY resize (`None` until the first frame).
    cell_px: Option<(u32, u32)>,
    /// ANSI-stripped recent output (bounded), used for yanking.
    capture: String,
    /// Start index in `capture` of the shell's last command output
    /// (used only when the shell emits no prompt markers).
    last_cmd: usize,
    /// Output offsets of OSC 133 prompt markers (shell integration).
    markers: Vec<usize>,
    /// The line currently being typed, and the last submitted command line,
    /// so the shell's echo of the command can be dropped when yanking.
    input_line: String,
    last_input: String,
    /// Most recent prompt text (e.g. `F:\x>`), used to strip a prompt glued
    /// to the end of the final output line.
    last_prompt: Option<String>,
    /// Working directory of the shell, tracked passively from its output
    /// (OSC 7 reports, cmd/PowerShell prompts -- so `cd`, `pushd` and the
    /// yazi `y.cmd` wrapper are all followed) plus the explicit
    /// [`Pane::request_cwd`] query as fallback.
    cwd: std::path::PathBuf,
    /// Bumped every time a cwd reply updates [`Pane::cwd`], so the UI can
    /// refresh completions exactly when the fresh value lands.
    cwd_seq: u64,
    /// Shell flavour, so the cwd query uses the right syntax.
    shell_kind: ShellKind,
    /// When the last chunk arrived, for the command-gap heuristic.
    last_out: std::time::Instant,
}

/// How the child shell spells a "print my cwd" command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Cmd,
    PowerShell,
    Posix,
}

/// Marker printed around the cwd so it can be parsed out of the output.
const CWD_MARK: &str = "SHELLRS_CWD:";

/// Cap on retained plain-text output per pane (~a few thousand lines).
const CAPTURE_CAP: usize = 512 * 1024;

/// Bytes after which a quiet gap starts a new command's output.
const CMD_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// ConPTY asks the terminal for the cursor position and blocks until it gets
/// an answer. Without this reply the shell never prints a prompt, so typing
/// appears broken.
const DSR: &[u8] = b"\x1b[6n";

/// Generic device-status query (`CSI 5 n`): TUIs (e.g. `ratatui-image`'s
/// capability probe) end their query burst with this and wait for *some*
/// reply before giving up. Answer `CSI 0 n` ("OK, no malfunction") so the
/// probe terminates instead of hanging until its timeout.
const DSR_STATUS: &[u8] = b"\x1b[5n";
const DSR_STATUS_REPLY: &[u8] = b"\x1b[0n";

/// Cell-size query (`CSI 16 t`, XTWINOPS): reports one character cell in
/// pixels as `CSI 6 ; height ; width t`. Graphics probes (`ratatui-image`
/// `Picker::from_query_stdio`) need this for a correct halfblock aspect;
/// without it they fall back to a guessed 10x20 cell and images look
/// stretched on top of being cell-based.
const CELL_SIZE: &[u8] = b"\x1b[16t";

/// Bytes kept across chunks so a query split over two reads is still seen.
/// Longest query is `CELL_SIZE` (6 bytes); keep a little more for safety.
const QUERY_TAIL_KEEP: usize = 8;

/// Toggle shellrs's own Ctrl+C immunity (see `install_console_guard`).
#[cfg(windows)]
fn set_ctrlc_ignored(ignored: bool) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe {
        SetConsoleCtrlHandler(None, if ignored { 1 } else { 0 });
    }
}

/// Spawn the shell without passing on shellrs's Ctrl+C immunity.
///
/// `SetConsoleCtrlHandler(NULL, TRUE)` (our startup guard so a child's
/// console event cannot kill shellrs) is *inherited* by child processes. A
/// shell spawned while immunity is on keeps ignoring `CTRL_C_EVENT`, and so
/// does everything it launches -- including a `cargo run` server, which then
/// survives Ctrl+C. Interactive TUIs that read `0x03` from stdin (opencode)
/// still work, which is why this looked "semi working".
///
/// Clearing immunity for the duration of `spawn_command` (re-armed right
/// after, on both success and failure) gives the new shell normal Ctrl+C
/// handling while shellrs itself stays immune.
#[cfg(windows)]
fn spawn_without_ctrlc_inherit(
    slave: &Box<dyn portable_pty::SlavePty + Send>,
    cmd: portable_pty::CommandBuilder,
    shell: &str,
) -> Result<Box<dyn Child + Send + Sync>, String> {
    set_ctrlc_ignored(false);
    let result = slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {shell:?}: {e}"));
    set_ctrlc_ignored(true);
    result
}

/// Non-Windows spawn: no console control handlers to worry about.
#[cfg(not(windows))]
fn spawn_without_ctrlc_inherit(
    slave: &Box<dyn portable_pty::SlavePty + Send>,
    cmd: portable_pty::CommandBuilder,
    shell: &str,
) -> Result<Box<dyn Child + Send + Sync>, String> {
    slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {shell:?}: {e}"))
}

impl Pane {
    /// Spawn `shell` (empty = auto) in a new PTY, pumping output on its own
    /// tokio blocking thread (`rt`). Every delivered chunk also pings `wake`
    /// so the UI thread redraws without waiting for its poll heartbeat.
    pub fn spawn(
        id: usize,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &crossbeam_channel::Sender<()>,
    ) -> Result<Self, String> {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Self::spawn_in(id, shell, scrollback, rt, wake, &cwd)
    }

    /// Spawn like [`spawn`], but start the shell in `cwd` instead of the
    /// process directory (used by split so the new pane inherits the focused
    /// pane's directory). Falls back to the process directory when `cwd`
    /// does not exist, so a deleted directory never fails the split.
    pub fn spawn_in(
        id: usize,
        shell: &str,
        scrollback: usize,
        rt: &tokio::runtime::Handle,
        wake: &crossbeam_channel::Sender<()>,
        cwd: &std::path::Path,
    ) -> Result<Self, String> {
        let system = native_pty_system();
        let size = PtySize {
            rows: FALLBACK_ROWS,
            cols: FALLBACK_COLS,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = system
            .openpty(size)
            .map_err(|e| format!("openpty: {e}"))?;

        let (prog, args) = split_shell(shell);
        let shell_kind = detect_shell(&prog);
        let mut cmd = CommandBuilder::new(prog);
        cmd.args(args);
        let start_cwd = if cwd.is_dir() {
            cwd.to_path_buf()
        } else {
            log::debug!("spawn_in: {cwd:?} missing, using process directory");
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        };
        cmd.cwd(&start_cwd);
        // Advertise a capable terminal so shells enable color.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // cmd.exe prompt with an OSC 133;A marker ($E = ESC), so yank can
        // find exactly where a command's output starts. Other shells ignore
        // PROMPT, and users can override it in their own profile.
        if std::env::var_os("PROMPT").is_none() {
            cmd.env("PROMPT", "$E]133;A$E\\$P$G");
        }

        let child = spawn_without_ctrlc_inherit(&pair.slave, cmd, shell)?;
        drop(pair.slave);
        log::debug!("pane {id} spawned {shell:?}");

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("pty writer: {e}"))?;

        // Bounded so a runaway child cannot grow memory without limit; the
        // blocking reader simply waits when the UI falls behind.
        let (tx, rx) = bounded::<Chunk>(128);
        let wake = wake.clone();
        rt.spawn_blocking(move || {
            let mut buf = [0u8; 8192];
            // Tail of the previous read, so an OSC 7 report split across
            // reads is still seen whole (512 chars cover any real URI).
            let mut tail = String::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        // No data right now (ConPTY idles like this); back
                        // off only briefly so bursty shell output is picked
                        // up with ~1ms latency instead of a full frame.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Ok(n) => {
                        let raw = buf[..n].to_vec();
                        // Strip escape sequences here (not on the UI thread):
                        // panes do this in parallel on their own reader tasks.
                        let lossy = String::from_utf8_lossy(&raw).into_owned();
                        let (text, marks) = crate::ansi::strip_ansi_markers(&lossy);
                        // Scan `tail + lossy` so split OSC 7 reports survive;
                        // the extractor stops at an unterminated tail, which
                        // becomes next round's head.
                        let scan = format!("{tail}{lossy}");
                        let osc7 = crate::ansi::extract_osc7_dirs(&scan);
                        tail = char_tail(&scan, 512);
                        if tx.send(Chunk {
                            raw,
                            text,
                            marks,
                            osc7,
                        })
                        .is_err()
                        {
                            break;
                        }
                        // Nudge the UI thread: output is waiting. The extra
                        // redraw requests coalesce, so floods stay cheap.
                        let _ = wake.send(());
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
                    Err(_) => {
                        // Transient ConPTY blip: keep pumping instead of
                        // killing the pane's output forever.
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        });

        Ok(Self {
            id,
            title: if shell.is_empty() {
                "shell".into()
            } else {
                shell.into()
            },
            parser: vt100::Parser::new(FALLBACK_ROWS, FALLBACK_COLS, scrollback.max(1)),
            rx,
            writer,
            master: pair.master,
            child,
            size: (FALLBACK_COLS, FALLBACK_ROWS),
            dead: false,
            query_tail: Vec::new(),
            cell_px: None,
            capture: String::new(),
            last_cmd: 0,
            markers: Vec::new(),
            input_line: String::new(),
            last_input: String::new(),
            last_prompt: None,
            cwd: start_cwd,
            cwd_seq: 0,
            shell_kind,
            last_out: std::time::Instant::now(),
        })
    }

    /// Drain reader channel into vt100 parser; poll child exit.
    /// Returns bytes consumed (for workspace activity tracking).
    pub fn poll(&mut self) -> usize {
        let mut n = 0;
        while let Ok(chunk) = self.rx.try_recv() {
            n += chunk.raw.len();
            self.answer_queries(&chunk.raw);
            self.record_output(&chunk.text, chunk.marks, &chunk.osc7);
            self.parser.process(&chunk.raw);
        }
        if !self.dead {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.dead = true;
                    self.parser.process(b"\r\n[shellrs] shell exited\r\n");
                }
                _ => {}
            }
        }
        n
    }

    /// Append already-stripped text to the yank buffer, recording prompt
    /// markers and (without markers) where the most recent burst began.
    /// The stripping itself happens on the reader task.
    ///
    /// The tracked cwd follows the shell without asking it anything: OSC 7
    /// directory reports win when present, otherwise a cmd/PowerShell prompt
    /// (`F:\dir>`, `PS C:\dir>`) at the end of a line reveals the directory
    /// the shell just landed in -- including `cd`, `pushd` and the `yazi`
    /// `y.cmd` wrapper, which all end with `cd /d <dir>`. Candidates must
    /// exist on disk, so stray output that merely looks like a path is
    /// ignored. The explicit [`Pane::request_cwd`] query stays as a fallback
    /// (image-prompt completions).
    fn record_output(&mut self, text: &str, marks: Vec<usize>, osc7: &[std::path::PathBuf]) {
        if text.is_empty() && marks.is_empty() && osc7.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        if now.duration_since(self.last_out) > CMD_GAP {
            self.last_cmd = self.capture.len();
        }
        self.last_out = now;
        self.parse_cwd(&text);
        for dir in osc7 {
            if dir.is_dir() {
                self.cwd = dir.clone();
                self.cwd_seq = self.cwd_seq.wrapping_add(1);
            }
        }
        for line in text.lines() {
            if let Some(dir) = cwd_from_prompt_line(line) {
                if dir.is_dir() {
                    self.cwd = dir;
                    self.cwd_seq = self.cwd_seq.wrapping_add(1);
                }
            }
        }
        let base = self.capture.len();
        for m in marks {
            self.markers.push(base + m);
        }
        // Remember bare prompt lines (no spaces, short, promptish) so a
        // prompt glued to the final output line can be trimmed later.
        for line in text.lines() {
            let t = line.trim_end();
            if promptish(t) && t.len() <= 60 && !t.contains(' ') {
                self.last_prompt = Some(t.to_string());
            }
        }
        self.capture.push_str(&text);
        if self.capture.len() > CAPTURE_CAP {
            // Trim on a char boundary (multibyte text would otherwise panic
            // `String::drain`), keeping marker offsets in sync.
            let cut = char_floor(&self.capture, self.capture.len() - CAPTURE_CAP);
            self.capture.drain(..cut);
            self.last_cmd = self.last_cmd.saturating_sub(cut);
            self.markers.retain_mut(|m| {
                if *m >= cut {
                    *m -= cut;
                    true
                } else {
                    false
                }
            });
        }
    }

    /// Plain text of the shell's most recent command output, trimmed.
    ///
    /// With prompt markers, takes the text between the last two prompts and
    /// drops its first line (which holds the prompt and echoed command).
    /// Without markers, falls back to the last output burst with its final
    /// line (the next prompt) removed.
    pub fn last_output(&self) -> String {
        // Marker/`last_cmd` offsets are byte positions; clamp them onto char
        // boundaries before slicing (the capture holds arbitrary UTF-8).
        let end = char_floor(&self.capture, self.markers.last().copied().unwrap_or(self.capture.len()));
        let text = if self.markers.len() >= 2 {
            let start = char_floor(&self.capture, self.markers[self.markers.len() - 2]);
            let start = start.min(end);
            drop_first_line(&self.capture[start..end]).to_string()
        } else if let Some(&start) = self.markers.first() {
            let start = char_floor(&self.capture, start).min(end);
            drop_first_line(&self.capture[start..end]).to_string()
        } else {
            // No prompt markers (e.g. cmd.exe/Clink): work on the last output
            // burst, dropping the leading prompt/echo lines and any trailing
            // prompt - including one glued to the last output line.
            let burst = &self.capture[self.last_cmd.min(self.capture.len())..];
            let cmd = self.last_input.trim();
            let mut lines: Vec<String> = burst.lines().map(|l| l.to_string()).collect();
            // Trim a prompt suffix stuck to the final output line.
            if let Some(p) = self.last_prompt.as_deref() {
                if let Some(last) = lines.iter_mut().rev().find(|l| !l.trim().is_empty()) {
                    if last.ends_with(p) && last.trim_end().len() > p.len() {
                        let keep = last.len() - p.len();
                        last.truncate(keep);
                    }
                }
            }
            while lines
                .last()
                .is_some_and(|l| l.trim().is_empty() || promptish(l))
            {
                lines.pop();
            }
            while let Some(first) = lines.first() {
                if promptish(first) || (!cmd.is_empty() && first.trim() == cmd) {
                    lines.remove(0);
                } else {
                    break;
                }
            }
            lines.join("\n")
        };
        text.trim_matches('\n').trim_end().to_string()
    }

    /// Reply to terminal queries in `bytes`:
    /// - `ESC[6n`: cursor position (ConPTY blocks for `ESC[<row>;<col>R`
    ///   at startup; without it the shell stalls before its first prompt),
    /// - `ESC[5n`: device status (`ESC[0n` = OK; ends graphics-capability
    ///   probes instead of letting them hang until timeout),
    /// - `ESC[16t`: cell size in pixels (`ESC[6;<h>;<w>t`; lets TUIs size
    ///   halfblock art to the real cell aspect instead of a 10x20 guess).
    ///
    /// A chunk boundary can split a sequence, so the previous tail is
    /// prepended for scanning.
    fn answer_queries(&mut self, bytes: &[u8]) {
        let mut scan = Vec::with_capacity(self.query_tail.len() + bytes.len());
        scan.extend_from_slice(&self.query_tail);
        scan.extend_from_slice(bytes);
        let (dsr, status, cell) = count_queries(&scan);
        // Keep a short tail in case an escape is split across chunks.
        let keep = QUERY_TAIL_KEEP.min(scan.len());
        self.query_tail = scan[scan.len() - keep..].to_vec();
        if dsr == 0 && status == 0 && cell == 0 {
            return;
        }
        let mut reply = Vec::new();
        if dsr > 0 {
            let (row, col) = self.parser.screen().cursor_position();
            let report = format!("\x1b[{};{}R", row + 1, col + 1);
            for _ in 0..dsr {
                reply.extend_from_slice(report.as_bytes());
            }
        }
        for _ in 0..status {
            reply.extend_from_slice(DSR_STATUS_REPLY);
        }
        // Without known cell pixels there is nothing truthful to report
        // (a guess would distort images more than the probe's own default).
        if cell > 0 && let Some((w, h)) = self.cell_px {
            let report = format!("\x1b[6;{h};{w}t");
            for _ in 0..cell {
                reply.extend_from_slice(report.as_bytes());
            }
        }
        if !reply.is_empty() {
            let _ = self.writer.write_all(&reply);
            let _ = self.writer.flush();
        }
    }

    /// Remember the GPU cell size in pixels (pushed by the window host).
    /// Drives `CSI 16 t` replies and the pixel dimensions reported on PTY
    /// resize (`TIOCGWINSZ` `ws_xpixel`/`ws_ypixel`).
    pub fn set_cell_px(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.cell_px = Some((width, height));
        }
    }

    /// Resize PTY + parser when layout rect changes. Pixel dimensions come
    /// from the last known cell size so children see a truthful window size
    /// (halfblock/image probes use the `ioctl` fallback on Unix).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(2);
        if (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        let (pixel_width, pixel_height) = self
            .cell_px
            .map(|(w, h)| {
                (
                    (cols as u32 * w).min(u16::MAX as u32) as u16,
                    (rows as u32 * h).min(u16::MAX as u32) as u16,
                )
            })
            .unwrap_or((0, 0));
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width,
            pixel_height,
        });
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Send Ctrl+C to the foreground process.
    ///
    /// Always writes `0x03` (ETX) into the PTY: on Unix the line discipline
    /// turns it into `SIGINT`, and at a bare `cmd.exe` prompt it cancels the
    /// current line. That byte alone does NOT stop a running server on
    /// Windows (e.g. `node server.js`, `python -m http.server`, `ping -t`):
    /// when the foreground process is not the shell reading input, ConPTY
    /// drops the ETX instead of raising `CTRL_C_EVENT`. So on Windows this
    /// also delivers a real console control event to every process attached
    /// to the child's console (the shell plus its grandchildren).
    pub fn interrupt(&mut self) {
        if self.dead {
            return;
        }
        if self.parser.screen().scrollback() > 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        let _ = self.writer.write_all(&[0x03]);
        let _ = self.writer.flush();
        #[cfg(windows)]
        self.send_ctrl_c_event();
    }

    /// Deliver `CTRL_C_EVENT` to the child's console (Windows only).
    ///
    /// The child lives on its own (pseudo-)console, so the event is sent via
    /// a temporary attach: detach from our console (if any), attach to the
    /// child's, generate the event for process group 0 (everyone attached
    /// there), then detach and re-attach to our parent console when there is
    /// one. Our own process ignores Ctrl+C (see `install_console_guard`), so
    /// generating the event cannot kill shellrs itself.
    #[cfg(windows)]
    fn send_ctrl_c_event(&self) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows_sys::Win32::System::Console::{
            ATTACH_PARENT_PROCESS, AttachConsole, FreeConsole, GenerateConsoleCtrlEvent,
            SetConsoleCtrlHandler, SetStdHandle, CTRL_C_EVENT, STD_ERROR_HANDLE,
            STD_OUTPUT_HANDLE,
        };
        const GENERIC_WRITE: u32 = 0x4000_0000;
        let Some(pid) = self.child.process_id() else {
            return;
        };
        unsafe {
            // Must detach before attaching elsewhere; ignore errors (we may
            // have no console at all in release GUI launches).
            FreeConsole();
            if AttachConsole(pid) == 0 {
                // Child gone or unreachable: try to restore our console.
                AttachConsole(ATTACH_PARENT_PROCESS);
                return;
            }
            // Stay alive when the event fires (belt and braces; the startup
            // guard already ignores Ctrl+C for this process).
            SetConsoleCtrlHandler(None, 1);
            // NOTE: CTRL_C_EVENT cannot target a group: the pid argument must
            // be 0 (everyone attached to the child's console). A nonzero id
            // succeeds but delivers nothing.
            let ok = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
            if ok == 0 {
                log::debug!("interrupt: GenerateConsoleCtrlEvent failed, pid={pid}");
            }
            FreeConsole();
            // Restore the parent console when launched from a terminal
            // (`cargo run`); no-op from Explorer. Rewire stdout/stderr onto
            // it like `attach_parent_console` in `main.rs`.
            if AttachConsole(ATTACH_PARENT_PROCESS) != 0 {
                let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
                let handle = CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if handle != INVALID_HANDLE_VALUE {
                    SetStdHandle(STD_OUTPUT_HANDLE, handle);
                    SetStdHandle(STD_ERROR_HANDLE, handle);
                }
            }
        }
    }

    /// Write raw bytes to child stdin.
    pub fn write(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.dead {
            return;
        }
        // A raw ETX reaching `write` directly (paste, macro) gets the same
        // console event as a Ctrl+C keypress, so it can also stop a server.
        if bytes == [0x03] {
            self.interrupt();
            return;
        }
        // Typing jumps back to the live screen (common terminal behavior).
        if self.parser.screen().scrollback() > 0 {
            self.parser.screen_mut().set_scrollback(0);
        }
        self.track_input(bytes);
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Track the line being typed so the shell's echo can be recognized.
    /// Approximate: printable bytes append, DEL deletes, CR/NL submits.
    fn track_input(&mut self, bytes: &[u8]) {
        for &b in bytes {
            match b {
                0x7f | 0x08 => {
                    self.input_line.pop();
                }
                b'\r' | b'\n' => {
                    let line = self.input_line.trim().to_string();
                    if !line.is_empty() {
                        self.last_input = line;
                    }
                    self.input_line.clear();
                }
                // Skip escapes/controls (arrows, ctrl combos) entirely.
                0x00..=0x1f => {}
                _ => self.input_line.push(b as char),
            }
        }
    }

    /// Ask the shell to print its working directory (marked for parsing).
    /// The reply arrives asynchronously and updates [`Pane::cwd`].
    pub fn request_cwd(&mut self) {
        let cmd = match self.shell_kind {
            ShellKind::Cmd => format!("echo {CWD_MARK}%CD%"),
            ShellKind::PowerShell => format!("echo {CWD_MARK}$($PWD.Path)"),
            ShellKind::Posix => format!("echo {CWD_MARK}$PWD"),
        };
        self.write(format!("{cmd}\r").as_bytes());
    }

    /// Scan freshly received text for a cwd marker reply.
    fn parse_cwd(&mut self, text: &str) {
        for line in text.lines() {
            let Some(idx) = line.find(CWD_MARK) else {
                continue;
            };
            let value = line[idx + CWD_MARK.len()..].trim();
            // Skip the echoed command itself (unexpanded `%CD%` / `$PWD`).
            if value.is_empty() || value.contains('%') || value.contains('$') {
                continue;
            }
            self.cwd = std::path::PathBuf::from(value);
            self.cwd_seq = self.cwd_seq.wrapping_add(1);
        }
    }

    /// Test hook: feed stripped output through the same path live PTY bytes
    /// take (marker + OSC 7 + prompt tracking, without the vt100 parser).
    #[cfg(test)]
    pub(crate) fn feed_text_for_test(&mut self, text: &str, osc7: &[std::path::PathBuf]) {
        self.record_output(text, Vec::new(), osc7);
    }

    /// Monotonic identifier of the last cwd update (see [`Pane::request_cwd`]).
    pub fn cwd_seq(&self) -> u64 {
        self.cwd_seq
    }

    /// Working directory last reported by the shell.
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// What mouse reports the hosted app wants, from its DECSET tracking
    /// mode (9 press-only, 1000 press+release, 1002 +drag, 1003 +free
    /// motion) and encoding flag (1006/1005/default).
    pub fn mouse_state(&self) -> crate::mouse::MouseState {
        use crate::mouse::{MouseEncoding, MouseState};
        use vt100::{MouseProtocolEncoding as E, MouseProtocolMode as M};
        let screen = self.screen();
        let (tracking, release, any_motion, drag_motion) = match screen.mouse_protocol_mode() {
            M::None => (false, false, false, false),
            M::Press => (true, false, false, false),
            M::PressRelease => (true, true, false, false),
            M::ButtonMotion => (true, true, false, true),
            M::AnyMotion => (true, true, true, true),
        };
        let encoding = match screen.mouse_protocol_encoding() {
            E::Sgr => MouseEncoding::Sgr,
            E::Utf8 => MouseEncoding::Utf8,
            E::Default => MouseEncoding::X10,
        };
        MouseState {
            tracking,
            release,
            any_motion,
            drag_motion,
            encoding,
        }
    }

    /// Set the tracked cwd directly (tests).
    #[cfg(test)]
    pub fn set_cwd_for_test(&mut self, path: std::path::PathBuf) {
        self.cwd = path;
    }

    /// Scroll the emulated screen's scrollback by `delta` rows (positive =
    /// up into history); clamped by vt100.
    pub fn scroll(&mut self, delta: i32) {
        let s = self.parser.screen_mut();
        let cur = s.scrollback() as i32;
        s.set_scrollback((cur + delta).max(0) as usize);
    }

    /// Jump back to the live screen.
    #[allow(dead_code)]
    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Type pasted text, wrapping it in bracketed-paste markers when the
    /// foreground program asked for them.
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() || self.dead {
            return;
        }
        let bracketed = self.parser.screen().bracketed_paste();
        let bytes = paste_bytes(text, bracketed);
        self.write(&bytes);
    }

    /// Kill the child shell (best effort). Test cleanup uses this so shells
    /// never outlive the suite and pile up ConPTY pressure for later tests.
    #[cfg(test)]
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Current emulated screen (read-only for renderer).
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Feed bytes straight into the parser (tests only).
    #[cfg(test)]
    pub(crate) fn feed_for_test(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Visible grid as rows padded to the pane width (for select mode).
    /// Uses display width so wide glyphs occupy the right number of columns.
    pub fn grid_lines(&self) -> Vec<String> {
        use unicode_width::UnicodeWidthStr;
        let s = self.parser.screen();
        let (rows, cols) = s.size();
        let mut out = Vec::with_capacity(rows as usize);
        for r in 0..rows {
            let mut line = String::new();
            let mut used = 0usize;
            for c in 0..cols {
                let Some(cell) = s.cell(r, c) else { continue };
                let text = cell.contents();
                // vt100 reports blank cells as ""; emit a real space so
                // columns stay aligned instead of collapsing the row.
                let (chunk, w) = if text.is_empty() {
                    (" ", 1usize)
                } else {
                    (text, text.width().max(1))
                };
                if used + w > cols as usize {
                    break;
                }
                line.push_str(chunk);
                used += w;
            }
            while used < cols as usize {
                line.push(' ');
                used += 1;
            }
            out.push(line);
        }
        out
    }
}

/// Serializes tests that spawn real shells: parallel ConPTY + `cmd.exe`
/// spawn storms intermittently stall for a minute or more (conhost startup
/// serialization / AV scanning), so PTY tests take this guard.
#[cfg(test)]
pub(crate) static PTY_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock [`PTY_TEST_GUARD`], recovering from a poisoned predecessor so one
/// panicking PTY test cannot wedge the rest of the suite.
#[cfg(test)]
pub(crate) fn lock_pty_tests() -> std::sync::MutexGuard<'static, ()> {
    PTY_TEST_GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Count terminal queries in `data`: `(cursor DSR, status DSR, cell-size)`.
/// Pure helper so the scanning is unit-testable without a live PTY.
fn count_queries(data: &[u8]) -> (usize, usize, usize) {
    let count = |needle: &[u8]| {
        if data.len() < needle.len() {
            return 0;
        }
        data.windows(needle.len()).filter(|w| *w == needle).count()
    };
    (count(DSR), count(DSR_STATUS), count(CELL_SIZE))
}

/// Mark pasted text with bracketed-paste markers when requested.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        let mut buf = Vec::with_capacity(text.len() + 12);
        buf.extend_from_slice(b"\x1b[200~");
        buf.extend_from_slice(text.as_bytes());
        buf.extend_from_slice(b"\x1b[201~");
        buf
    } else {
        text.as_bytes().to_vec()
    }
}

/// Largest char-boundary index `<= i` (never panics, never splits UTF-8).
fn char_floor(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        i = s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Last `keep` characters of `s` (char-boundary safe), for carrying a split
/// escape sequence across PTY reads.
fn char_tail(s: &str, keep: usize) -> String {
    if s.chars().count() <= keep {
        return s.to_string();
    }
    let skip = s.chars().count() - keep;
    let idx = s
        .char_indices()
        .nth(skip)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    s[idx..].to_string()
}

/// Directory shown by a trailing cmd/PowerShell prompt on `line`, if any.
///
/// Matches a drive (`C:\dir>`, also glued like `outputC:\dir>`) or UNC
/// (`\\srv\share>`) path ending the trimmed line at `>`, plus Unix paths
/// behind a PowerShell `PS ` prefix (`PS /home/u>`). Pure function: callers
/// check `is_dir` so prompt-like output never moves the tracked cwd.
fn cwd_from_prompt_line(line: &str) -> Option<std::path::PathBuf> {
    let t = line.trim_end();
    // Shortest real prompt is `C:\>`; cap length so hostile output stays cheap.
    if !t.ends_with('>') || t.len() < 4 || t.len() > 512 {
        return None;
    }
    let body = &t[..t.len() - 1];
    let b = body.as_bytes();
    // Last `X:\` / `X:/` opener anywhere (handles prompts glued to output).
    let mut drive: Option<usize> = None;
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i].is_ascii_alphabetic() && b[i + 1] == b':' && (b[i + 2] == b'\\' || b[i + 2] == b'/')
        {
            drive = Some(i);
        }
        i += 1;
    }
    // Last UNC opener.
    let unc = body.rfind("\\\\");
    let start = match (drive, unc) {
        (Some(d), Some(u)) => Some(d.max(u)),
        (Some(d), None) => Some(d),
        (None, Some(u)) => Some(u),
        (None, None) => unix_prompt_path(body),
    }?;
    let candidate = body[start..].trim();
    if !looks_like_dir(candidate) {
        return None;
    }
    Some(std::path::PathBuf::from(candidate))
}

/// Start index of a Unix prompt path: `PS /home/u` (PowerShell prefix) or a
/// whole-line absolute path (`/home/u`). Glued output is not handled -- the
/// drive/UNC arms above own the general case.
fn unix_prompt_path(body: &str) -> Option<usize> {
    if let Some(rest) = body
        .strip_prefix("PS ")
        .or_else(|| body.strip_prefix("PS\t"))
    {
        if rest.starts_with('/') {
            return Some(body.len() - rest.len());
        }
        return None;
    }
    if body.starts_with('/') {
        return Some(0);
    }
    None
}

/// Shape check before the `is_dir` syscall: absolute-looking, bounded, and
/// free of characters Windows paths can never contain.
fn looks_like_dir(candidate: &str) -> bool {
    if candidate.is_empty() || candidate.len() > 32767 {
        return false;
    }
    let b = candidate.as_bytes();
    let absolute = (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || candidate.starts_with("\\\\")
        || candidate.starts_with('/');
    if !absolute {
        return false;
    }
    if candidate.contains(['<', '>', '|', '?', '*', '"']) {
        return false;
    }
    if candidate.chars().any(|c| c.is_control()) {
        return false;
    }
    if candidate.starts_with("\\\\") {
        // `\\server\share` at minimum: ["", "", server, share].
        let parts: Vec<&str> = candidate.split('\\').collect();
        if parts.len() < 4 || parts[2].is_empty() || parts[3].is_empty() {
            return false;
        }
    }
    true
}

/// Guess the shell flavour from the program name, to pick cwd-query syntax.
fn detect_shell(prog: &str) -> ShellKind {
    let name = std::path::Path::new(prog)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(prog)
        .to_ascii_lowercase();
    if name.contains("cmd") {
        ShellKind::Cmd
    } else if name.contains("pwsh") || name.contains("powershell") {
        ShellKind::PowerShell
    } else {
        ShellKind::Posix
    }
}

/// Everything after the first newline (the prompt/echo line).
fn drop_first_line(s: &str) -> &str {
    match s.find('\n') {
        Some(i) => &s[i + 1..],
        None => "",
    }
}

/// Heuristic: does this look like a shell prompt line?
/// Matches cmd (`F:\x>`), bash/zsh (`$`/`%`/`#`), and PowerShell (`PS C:\>`).
fn promptish(line: &str) -> bool {
    let t = line.trim_end();
    if t.is_empty() {
        return false;
    }
    matches!(t.chars().last(), Some('>') | Some('$') | Some('%') | Some('#'))
}

#[cfg(test)]
mod query_tests {
    use super::{CELL_SIZE, DSR, DSR_STATUS, QUERY_TAIL_KEEP, count_queries};

    #[test]
    fn counts_each_query_kind() {
        // Cursor DSR, status DSR and cell-size side by side.
        let mut burst = Vec::new();
        burst.extend_from_slice(b"hello");
        burst.extend_from_slice(DSR);
        burst.extend_from_slice(DSR_STATUS);
        burst.extend_from_slice(CELL_SIZE);
        burst.extend_from_slice(DSR);
        assert_eq!(count_queries(&burst), (2, 1, 1));
    }

    #[test]
    fn ignores_lookalikes_and_own_replies() {
        // Our own replies must not re-trigger: CPR `ESC[5;1R` is not
        // `ESC[5n`, and the cell reply `ESC[6;16;8t` is not `ESC[6n`.
        assert_eq!(count_queries(b"\x1b[5;1R"), (0, 0, 0));
        assert_eq!(count_queries(b"\x1b[6;16;8t"), (0, 0, 0));
        assert_eq!(count_queries(b"\x1b[0n"), (0, 0, 0));
        assert_eq!(count_queries(b"plain output"), (0, 0, 0));
        assert_eq!(count_queries(b"\x1b["), (0, 0, 0));
    }

    #[test]
    fn split_query_survives_chunk_boundary() {
        // `ESC[16t` split as `ESC[1` + `6t`: trailing tail + head rescan.
        let (head, tail) = CELL_SIZE.split_at(3);
        // First chunk alone: nothing complete yet.
        assert_eq!(count_queries(head), (0, 0, 0));
        // Tail kept from the first chunk + second chunk completes it.
        let keep = QUERY_TAIL_KEEP.min(head.len());
        let mut joined = Vec::from(&head[head.len() - keep..]);
        joined.extend_from_slice(tail);
        assert_eq!(count_queries(&joined), (0, 0, 1));
    }

    #[test]
    fn cell_reply_format_matches_xtwinops() {
        // `CSI 16 t` answers `CSI 6 ; height ; width t`.
        let (w, h) = (8u32, 16u32);
        let report = format!("\x1b[6;{h};{w}t");
        assert_eq!(report, "\x1b[6;16;8t");
    }
}

#[cfg(test)]
mod yank_tests {
    use super::drop_first_line;

    #[test]
    fn drops_prompt_and_echo_line() {
        // Segment between two OSC markers: prompt line, then output.
        assert_eq!(drop_first_line("PS> echo hi\nhi\n"), "hi\n");
        assert_eq!(drop_first_line("no newline"), "");
        assert_eq!(drop_first_line("a\nb"), "b");
    }

    #[test]
    fn char_floor_never_splits_utf8() {
        let s = "aé"; // 'é' is 2 bytes: boundaries 0,1,3
        assert_eq!(super::char_floor(s, 2), 1);
        assert_eq!(super::char_floor(s, 3), 3);
        assert_eq!(super::char_floor(s, 99), 3);
        assert_eq!(super::char_floor(s, 0), 0);
    }

    #[test]
    fn detects_shell_flavour() {
        use super::{ShellKind, detect_shell};
        assert_eq!(detect_shell("C:\\Windows\\System32\\cmd.exe"), ShellKind::Cmd);
        assert_eq!(detect_shell("pwsh"), ShellKind::PowerShell);
        assert_eq!(detect_shell("/bin/bash"), ShellKind::Posix);
    }

    #[test]
    fn prompt_line_parsing() {
        use super::cwd_from_prompt_line;
        let p = |s: &str| cwd_from_prompt_line(s).map(|p| p.to_string_lossy().into_owned());
        // cmd, PowerShell, glued output, UNC.
        assert_eq!(p("F:\\work>"), Some("F:\\work".into()));
        assert_eq!(p("PS C:\\work>"), Some("C:\\work".into()));
        assert_eq!(p("downloaded 100%C:\\work>"), Some("C:\\work".into()));
        assert_eq!(p("\\\\srv\\share>"), Some("\\\\srv\\share".into()));
        assert_eq!(p("PS /home/u>"), Some("/home/u".into()));
        assert_eq!(p("C:/work>"), Some("C:/work".into()));
        // Not prompts: wrong terminator, relative, junk, bare markers.
        assert_eq!(p("F:\\work$"), None);
        assert_eq!(p("work>"), None);
        assert_eq!(p(">"), None);
        assert_eq!(p(""), None);
        assert_eq!(p("\\\\srv>"), None);
        assert_eq!(p("see /tmp>"), None);
        assert_eq!(p("C:\\a|b>"), None);
    }
}

/// Split `"prog arg1 arg2"`; empty -> platform default shell.
fn split_shell(shell: &str) -> (String, Vec<String>) {
    let shell = shell.trim();
    if shell.is_empty() {
        return (default_shell(), Vec::new());
    }
    let mut parts = shell.split_whitespace();
    let prog = parts.next().unwrap_or("sh").to_string();
    let args = parts.map(|s| s.to_string()).collect();
    (prog, args)
}

/// Platform default shell.
fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Probe raw PTY reads on a background thread (bounded wait), logging
    /// each result plus spawn diagnostics.
    #[test]
    #[ignore = "diagnostic probe; run with --ignored"]
    fn raw_read_probe() {
        println!("COMSPEC={:?}", std::env::var("COMSPEC"));
        println!("default_shell={:?}", default_shell());
        let system = native_pty_system();
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = system.openpty(size).expect("openpty");
        let mut cmd = CommandBuilder::new(default_shell());
        cmd.arg("/c");
        cmd.arg("echo hello_from_pty");
        cmd.cwd(std::env::current_dir().unwrap());
        let mut child = pair.slave.spawn_command(cmd).expect("spawn");
        println!("spawned, pid={:?}", child.process_id());
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().expect("reader");
        let mut writer = pair.master.take_writer().expect("writer");
        let _ = writer.write_all(b"echo probe123\r");
        let _ = writer.flush();

        let (tx, rx) = crossbeam_channel::unbounded::<String>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send("Ok(0)".into());
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(n) => {
                        if tx
                            .send(format!("Ok({n}) {:?}", String::from_utf8_lossy(&buf[..n])))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(format!("Err({e}) kind={:?}", e.kind()));
                        break;
                    }
                }
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(msg) => {
                    got += 1;
                    println!("recv: {msg}");
                }
                Err(_) => {}
            }
        }
        println!("wait_status={:?}", child.try_wait());
        println!("messages={got}");
    }

    /// Probe: does writing to the PTY reach the shell and come back?
    #[test]
    fn shell_echoes_input() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn pane");
        // Give the shell a moment to print its banner.
        for _ in 0..40 {
            pane.poll();
            std::thread::sleep(Duration::from_millis(25));
        }
        pane.write(b"echo shellrs_probe\r");
        let mut seen = false;
        for _ in 0..200 {
            pane.poll();
            if pane.screen().contents().contains("shellrs_probe") {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(seen, "shell never echoed input; screen:\n{}", pane.screen().contents());
        pane.kill();
        rt.shutdown_background();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shellrs-pty-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `spawn_in` starts the shell in the given directory (tracked cwd
    /// included); a missing directory falls back to the process directory
    /// instead of failing the spawn.
    #[test]
    fn spawn_in_starts_shell_in_given_dir() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let dir = temp_dir("spawn-in");
        let mut pane = Pane::spawn_in(0, "", 200, rt.handle(), &wake, &dir).expect("spawn in dir");
        assert_eq!(pane.cwd(), dir.as_path());
        pane.kill();

        let missing = dir.join("gone-missing");
        let mut fallback =
            Pane::spawn_in(1, "", 200, rt.handle(), &wake, &missing).expect("spawn falls back");
        assert_eq!(
            fallback.cwd(),
            std::env::current_dir().unwrap().as_path()
        );
        fallback.kill();
        let _ = std::fs::remove_dir_all(&dir);
        rt.shutdown_background();
    }

    /// With the OSC 133 prompt marker, yank returns only the command output.
    #[test]
    fn yank_excludes_prompt_and_echo() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        // Let the first prompt (with its marker) arrive.
        for _ in 0..60 {
            pane.poll();
            std::thread::sleep(Duration::from_millis(25));
            if !pane.markers.is_empty() {
                break;
            }
        }
        pane.write(b"echo hello world\r");
        let mut text = String::new();
        for _ in 0..200 {
            pane.poll();
            text = pane.last_output();
            if text.trim() == "hello world" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        pane.kill();
        let got = text.trim();
        assert_eq!(got, "hello world", "prompt/echo stripped (capture={:?})", pane.capture);
        rt.shutdown_background();
    }

    /// The cwd query parses the shell's answer (and ignores its own echo).
    #[test]
    fn cwd_query_parses_shell_reply() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        pane.parse_cwd("echo SHELLRS_CWD:%CD%\r\n");
        assert_eq!(pane.cwd(), std::env::current_dir().unwrap().as_path());
        pane.parse_cwd("SHELLRS_CWD:C:\\Users\\me\\project\r\n");
        assert_eq!(pane.cwd(), std::path::Path::new("C:\\Users\\me\\project"));
        pane.kill();
        rt.shutdown_background();
    }

    /// Prompts move the tracked cwd (what `cd`, `pushd` and the yazi
    /// `y.cmd` wrapper print), while lookalikes and gone dirs do not.
    #[test]
    fn prompt_lines_move_tracked_cwd() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        let base = std::env::temp_dir().join(format!(
            "shellrs-prompt-cwd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let dir_a = base.join("a");
        let dir_b = base.join("my dir");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let seq0 = pane.cwd_seq();

        // Plain cmd prompt, then a `cd` to another dir (incl. a space).
        pane.feed_text_for_test(&format!("{}\r\n{}>", dir_a.display(), dir_a.display()), &[]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        assert!(pane.cwd_seq() != seq0);
        pane.feed_text_for_test(
            &format!("cd /d \"{}\"\r\n{}>", dir_b.display(), dir_b.display()),
            &[],
        );
        assert_eq!(pane.cwd(), dir_b.as_path());
        // PowerShell shape and glued output both resolve.
        pane.feed_text_for_test(&format!("PS {}>", dir_a.display()), &[]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        pane.feed_text_for_test(&format!("done{}>", dir_b.display()), &[]);
        assert_eq!(pane.cwd(), dir_b.as_path());
        // OSC 7 reports apply too.
        pane.feed_text_for_test("", std::slice::from_ref(&dir_a));
        assert_eq!(pane.cwd(), dir_a.as_path());

        // Gone dirs, bare markers and ordinary output are ignored.
        let seq = pane.cwd_seq();
        pane.feed_text_for_test("Z:\\definitely-missing-shellrs>\r\n", &[]);
        pane.feed_text_for_test(">\r\n$\r\nsee you later>\r\n", &[]);
        pane.feed_text_for_test("", &[std::path::PathBuf::from("Z:\\missing-too")]);
        assert_eq!(pane.cwd(), dir_a.as_path());
        assert_eq!(pane.cwd_seq(), seq);
        pane.kill();
        let _ = std::fs::remove_dir_all(&base);
        rt.shutdown_background();
    }

    #[test]
    fn paste_wraps_in_bracketed_markers_when_enabled() {
        // Raw text when bracketed mode is off.
        assert_eq!(paste_bytes("hi", false), b"hi".to_vec());
        // Wrapped when the program enabled bracketed paste.
        assert_eq!(
            paste_bytes("hi", true),
            b"\x1b[200~hi\x1b[201~".to_vec()
        );
    }

    #[test]
    fn bracketed_paste_mode_follows_the_stream() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        assert!(!pane.screen().bracketed_paste());
        pane.feed_for_test(b"\x1b[?2004h");
        assert!(pane.screen().bracketed_paste(), "enabled by the program");
        pane.feed_for_test(b"\x1b[?2004l");
        assert!(!pane.screen().bracketed_paste(), "disabled again");
        pane.kill();
        rt.shutdown_background();
    }

    /// DECSET mouse modes map onto report behavior: 9 press-only, 1000
    /// press+release, 1002 adds drag motion, 1003 adds free motion, and
    /// 1006 picks SGR encoding.
    #[test]
    fn mouse_state_follows_decset() {
        use crate::mouse::MouseEncoding;
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let mut pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn");
        let off = pane.mouse_state();
        assert!(!off.tracking, "no tracking by default");
        assert_eq!(off.encoding, MouseEncoding::X10);

        // DECSET 9 (X10): presses only.
        pane.feed_for_test(b"\x1b[?9h");
        let press = pane.mouse_state();
        assert!(press.tracking);
        assert!(!press.release, "9 reports no releases");
        assert!(!press.wants_motion(false) && !press.wants_motion(true));
        pane.feed_for_test(b"\x1b[?9l");

        // DECSET 1000: press + release.
        pane.feed_for_test(b"\x1b[?1000h");
        let rel = pane.mouse_state();
        assert!(rel.tracking && rel.release);
        assert!(!rel.wants_motion(false) && !rel.wants_motion(true));

        pane.feed_for_test(b"\x1b[?1006h");
        let sgr = pane.mouse_state();
        assert!(sgr.tracking && sgr.release);
        assert_eq!(sgr.encoding, MouseEncoding::Sgr);

        // DECSET 1002: drag motion while held, but no free motion.
        pane.feed_for_test(b"\x1b[?1002h");
        let drag = pane.mouse_state();
        assert!(drag.tracking && drag.release);
        assert!(!drag.wants_motion(false), "no free motion in 1002");
        assert!(drag.wants_motion(true), "drag motion in 1002");

        // DECSET 1003: motion regardless of buttons.
        pane.feed_for_test(b"\x1b[?1003h");
        let motion = pane.mouse_state();
        assert!(motion.tracking && motion.release);
        assert!(motion.wants_motion(false), "any-motion reports freely");
        assert!(motion.wants_motion(true));

        pane.feed_for_test(b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l");
        assert!(!pane.mouse_state().tracking, "disabled again");
        pane.kill();
        rt.shutdown_background();
    }

    #[test]
    fn grid_lines_cover_pane_width() {
        let _guard = crate::pty::lock_pty_tests();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (wake, _rx) = crossbeam_channel::unbounded::<()>();
        let pane = Pane::spawn(0, "", 200, rt.handle(), &wake).expect("spawn pane");
        let size = pane.screen().size();
        let lines = pane.grid_lines();
        let mut pane = pane;
        pane.kill();
        assert_eq!(lines.len(), size.0 as usize, "one line per row");
        assert_eq!(
            lines.first().map(|l| l.chars().count()),
            Some(size.1 as usize),
            "each line padded to the pane width, size={size:?}"
        );
        rt.shutdown_background();
    }
}
