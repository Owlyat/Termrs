//! File logger on the `log` facade: `shellrs::*` records at debug level,
//! third-party crates at warn, so GPU shader chatter never buries our lines.
//! Active in debug builds or with `--debug`; the file is `shellrs.log` in
//! the config directory (see `Config::log_path`).

use std::fs::File;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// Logger writing `elapsed level target: args` lines to one file.
struct FileLogger {
    file: Mutex<File>,
    start: std::time::Instant,
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let max = if metadata.target().starts_with("shellrs") {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Warn
        };
        metadata.level() <= max
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let elapsed = self.start.elapsed();
        let line = format!(
            "{}.{:03} [{}] {}: {}\n",
            elapsed.as_secs(),
            elapsed.subsec_millis(),
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

/// Install the file logger once; failures fall back to stderr only.
pub fn init(path: &std::path::Path) {
    static DONE: OnceLock<()> = OnceLock::new();
    if DONE.set(()).is_err() {
        eprintln!("shellrs: logger already initialized");
        return;
    }
    install_panic_hook();
    match File::create(path) {
        Ok(file) => {
            let logger: &'static FileLogger = Box::leak(Box::new(FileLogger {
                file: Mutex::new(file),
                start: std::time::Instant::now(),
            }));
            if log::set_logger(logger).is_err() {
                eprintln!("shellrs: logger already initialized");
            } else {
                log::set_max_level(log::LevelFilter::Debug);
                log::debug!("logging to {}", path.display());
            }
        }
        Err(e) => eprintln!("shellrs: cannot open log {}: {e}", path.display()),
    }
}

/// Route panics into the log so crashes are captured even in a GUI window.
/// Also installs the hook when logging is disabled, so the message is at
/// least printed with a clear prefix.
pub fn install_panic_hook() {
    static ONCE: OnceLock<()> = OnceLock::new();
    if ONCE.set(()).is_err() {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        log::error!("PANIC: {info}\n{bt}");
        eprintln!("shellrs panic: {info}");
    }));
}

/// Ignore console Ctrl+C. A GUI app launched from a terminal (e.g. `cargo run`)
/// shares that console, and a child process (an AI CLI, a shell) can deliver a
/// console control event that would otherwise kill shellrs with
/// `STATUS_CONTROL_C_EXIT`. Close the window to quit instead.
///
/// NOTE: this immunity is inherited by child processes at spawn time, which
/// would make them ignore Ctrl+C too. `pty.rs` therefore clears it across
/// `spawn_command` and re-arms it right after, so shells/servers get normal
/// handling while shellrs itself stays immune.
pub fn install_console_guard() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        // A NULL handler registered as the only one makes the process ignore
        // Ctrl+C while children still receive it.
        let ok = unsafe { SetConsoleCtrlHandler(None, 1) };
        if ok == 0 {
            eprintln!("shellrs: could not install console ctrl+c guard");
        }
    }
}
