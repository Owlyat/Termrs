//! `shellrs` - a terminal emulator that opens its own native window.
//!
//! Panes spawn your shell in a `portable-pty`, output is parsed with `vt100`,
//! and the whole UI is drawn with ratatui into a `winit` + `wgpu` window via
//! `ratatui-wgpu` (no external terminal required). `kalk`, `lp` and `piper`
//! are ordinary commands you type into a pane.
//!
//! Keybindings come from `config.toml` (see `--print-default-config`), and
//! the workspace/pane layout is saved to `layout.toml` (next to the config)
//! and restored on the next run.

// No console window in release builds on Windows: this is a GUI app and the
// extra conhost window is just clutter. Debug builds keep the console so
// `cargo run` output, logs and CLI flags stay visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai;
mod ansi;
mod app;
mod cheatsheet;
mod clipboard;
mod commands_db;
mod config;
mod font;
mod image_gpu;
mod image_view;
mod ipc;
mod keys;
mod layout;
mod logging;
mod mouse;
mod pty;
mod scrollback;
mod server;
mod ui;
mod window;
mod workspace;

use clap::Parser;

const LONG_ABOUT: &str = "\
Split-pane terminal emulator that opens its own native window
(winit + wgpu via ratatui-wgpu; no external terminal needed).
Workspaces are tabs shown in the top bar, each with its own split layout;
the layout is saved to layout.toml and restored on the next run.
The inbox menu (ctrl+o) lists every workspace with live status: alive
panes, last finished command, and an unread-output dot.
kalk, lp and piper are run as normal shell commands inside a pane.
Views: shift+pageup scrollback, ctrl+i image, f1 big-text help.
Window: [window] background_image, opacity (0..1 over the image),
decorations = false for a borderless window.
Font: set [general] font to a .ttf path (else auto-detected).
Logging: --debug writes shellrs.log next to the config; --debug-dir <dir>
writes it there instead (created if missing). Panics are logged too.
All keys are configurable in [keys] (see --print-default-config).";

/// Command-line interface (parsed with clap).
#[derive(Parser, Debug)]
#[command(
    name = "shellrs",
    version,
    about = "Split-pane terminal emulator with its own native window.",
    long_about = LONG_ABOUT
)]
struct Cli {
    /// Config file path (default: ~/.config/shellrs/config.toml, created
    /// with built-in defaults on first run; falls back to cwd and next to
    /// the binary for development).
    #[arg(short = 'c', long)]
    config: Option<std::path::PathBuf>,
    /// Layout file path (default: layout.toml next to the config).
    #[arg(long)]
    layout: Option<std::path::PathBuf>,
    /// Print the default config.toml and exit.
    #[arg(long)]
    print_default_config: bool,
    /// Boot the window, render one frame, then exit.
    #[arg(long)]
    selftest: bool,
    /// Debug logging to shellrs.log in the config directory
    /// (on by default in debug builds).
    #[arg(long)]
    debug: bool,
    /// Write shellrs.log into this directory instead (created if missing).
    /// Implies debug logging.
    #[arg(long, value_name = "DIR")]
    debug_dir: Option<std::path::PathBuf>,
    /// IPC client: send a command to the running shellrs instance.
    #[command(subcommand)]
    cli: Option<CliCommand>,
    /// Start an HTTP server for remote control (e.g. --server localhost:12345).
    #[arg(long, value_name = "ADDR")]
    server: Option<String>,
    /// Internal: marks this process as the detached master (skips re-spawn).
    #[arg(long, hide = true)]
    master: bool,
}

/// IPC subcommands (communicate with the running shellrs instance).
#[derive(clap::Subcommand, Debug)]
enum CliCommand {
    /// Split the focused pane and run a command in the new pane.
    SplitPane {
        /// Command arguments (e.g. cargo run --release).
        #[arg(trailing_var_arg = true, required = true)]
        args: Vec<String>,
        /// Working directory for the new pane.
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
    },
    /// List all panes as JSON.
    List,
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    attach_parent_console();
    let cli = Cli::parse();
    if cli.print_default_config {
        print!("{}", config::Config::default_toml());
        return std::process::ExitCode::SUCCESS;
    }

    if let Some(cmd) = cli.cli {
        return run_cli(cmd);
    }

    if cli.server.is_some() && !cli.master {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            use std::process::Command;
            let exe = std::env::current_exe().unwrap_or_else(|_| "shellrs.exe".into());
            let mut args: Vec<String> = vec!["--server".into()];
            if let Some(ref addr) = cli.server {
                args.push(addr.clone());
            }
            if let Some(ref cfg) = cli.config {
                args.push("--config".into());
                args.push(cfg.to_string_lossy().to_string());
            }
            if let Some(ref layout) = cli.layout {
                args.push("--layout".into());
                args.push(layout.to_string_lossy().to_string());
            }
            if cli.debug {
                args.push("--debug".into());
            }
            if let Some(ref dir) = cli.debug_dir {
                args.push("--debug-dir".into());
                args.push(dir.to_string_lossy().to_string());
            }
            args.push("--master".into());
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            let _ = Command::new(exe)
                .args(&args)
                .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                .spawn();
            return std::process::ExitCode::SUCCESS;
        }
    }

    let cfg = config::Config::load(cli.config.as_deref());
    logging::install_panic_hook();
    logging::install_console_guard();
    if cli.debug || cfg!(debug_assertions) || cli.debug_dir.is_some() {
        init_file_logger(&cfg, cli.debug_dir.as_deref());
    }
    let result = if cli.selftest {
        window::run_selftest(cfg)
    } else {
        window::run(cfg, cli.layout, cli.server.as_deref())
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shellrs: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Run an IPC client command (split-pane or list).
fn run_cli(cmd: CliCommand) -> std::process::ExitCode {
    let req = match cmd {
        CliCommand::SplitPane { args, dir } => ipc::Request {
            cmd: "split-pane".into(),
            args,
            dir,
        },
        CliCommand::List => ipc::Request {
            cmd: "list".into(),
            args: vec![],
            dir: None,
        },
    };
    match ipc::send_request(&req) {
        Ok(resp) => {
            if resp.ok {
                if let Some(panes) = resp.panes {
                    println!("{}", serde_json::to_string_pretty(&panes).unwrap());
                } else if let Some(pane_id) = resp.pane_id {
                    println!("{{\"pane_id\": {pane_id}}}");
                }
                std::process::ExitCode::SUCCESS
            } else {
                eprintln!("shellrs: {}", resp.error.unwrap_or("unknown error".into()));
                std::process::ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("shellrs: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Reattach to the parent console in release builds on Windows, where this
/// binary has no console of its own (`windows_subsystem`). Rewires stdout
/// and stderr onto it so `--help`, `--version`, `--print-default-config`,
/// `--selftest` and error messages stay visible when launched from a
/// terminal. No-ops when already attached (debug builds, `cargo run`) or
/// when there is no parent console (Explorer launch): output then simply
/// goes nowhere, same as any GUI app.
#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, SetStdHandle, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    // GENERIC_WRITE (0x40000000); kept literal to avoid pulling another module.
    const GENERIC_WRITE: u32 = 0x4000_0000;
    unsafe {
        // Stdout already valid: running with a console, nothing to do.
        if !GetStdHandle(STD_OUTPUT_HANDLE).is_null() {
            return;
        }
        // No parent console to attach to.
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        // Reopen stdout/stderr on the parent console. Rust resolves the std
        // handles per write, so rebinding them here is sufficient.
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
        if handle == INVALID_HANDLE_VALUE {
            return;
        }
        SetStdHandle(STD_OUTPUT_HANDLE, handle);
        SetStdHandle(STD_ERROR_HANDLE, handle);
    }
}

/// Log to `<config-dir>/shellrs.log`, or `<debug-dir>/shellrs.log` when
/// `--debug-dir` was given. Failures fall back to stderr only.
fn init_file_logger(cfg: &config::Config, debug_dir: Option<&std::path::Path>) {
    let path = match debug_dir {
        Some(dir) => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("shellrs: cannot create {}: {e}", dir.display());
            }
            dir.join("shellrs.log")
        }
        None => cfg.log_path(),
    };
    logging::init(&path);
}
