//! IPC server: named pipe (Windows) / Unix socket (Unix) for external tools.
//!
//! Protocol: JSON request line -> JSON response line.
//! Commands: `split-pane` (split focused pane, run command), `list` (pane status).

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};

/// Request from an external CLI client.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Request {
    pub cmd: String,
    /// Command arguments for `split-pane` (e.g. ["cargo", "run"]).
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the new pane (defaults to focused pane's cwd).
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

/// Response back to the CLI client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panes: Option<Vec<PaneInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One pane's status (for `list`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub pane_id: usize,
    pub workspace: usize,
    pub title: String,
    pub dead: bool,
    pub cwd: String,
}

/// Commands the IPC server sends to the app.
#[derive(Debug, Clone)]
pub enum IpcCommand {
    SplitPane { args: Vec<String>, dir: Option<PathBuf> },
    ListPanes,
}

/// Handle returned by the IPC server (held by the app).
pub struct IpcServer {
    pub cmd_rx: Receiver<IpcCommand>,
    pub resp_tx: Sender<Response>,
}

/// Wrapper to make Windows pipe handles `Send`.
#[derive(Clone, Copy)]
struct SendHandle(usize);
unsafe impl Send for SendHandle {}

/// Get the IPC pipe/socket path for this process.
pub fn ipc_path() -> PathBuf {
    let id = std::process::id();
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\termrs-{id}"))
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir().join(format!("termrs-{id}"))
    }
}

/// Start the IPC server on a background thread. Returns the command receiver
/// and response sender for the app to use.
pub fn start_server() -> IpcServer {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<IpcCommand>();
    let (resp_tx, resp_rx) = crossbeam_channel::unbounded::<Response>();
    let path = ipc_path();

    std::thread::spawn(move || {
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawHandle;
            use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};

            use windows_sys::Win32::System::Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
                PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            };
            const PIPE_ACCESS_DUPLEX: u32 = 0x00000003;

            let pipe_name: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
            loop {
                let handle = unsafe {
                    CreateNamedPipeW(
                        pipe_name.as_ptr(),
                        PIPE_ACCESS_DUPLEX,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        PIPE_UNLIMITED_INSTANCES,
                        4096,
                        4096,
                        0,
                        std::ptr::null_mut(),
                    )
                };
                if handle == INVALID_HANDLE_VALUE {
                    log::error!("ipc: CreateNamedPipeW failed");
                    return;
                }
                log::info!("ipc: listening on {path:?}");
                let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
                if connected == 0 {
                    let err = std::io::Error::last_os_error();
                    if err.raw_os_error() == Some(233) {
                        unsafe { CloseHandle(handle) };
                        continue;
                    }
                    log::error!("ipc: ConnectNamedPipe failed: {err}");
                    unsafe { CloseHandle(handle) };
                    continue;
                }
                let cmd_tx = cmd_tx.clone();
                let resp_rx = resp_rx.clone();
                let handle = SendHandle(handle as usize);
                std::thread::spawn(move || {
                    let SendHandle(handle) = handle;
                    let mut reader = BufReader::new(unsafe { std::fs::File::from_raw_handle(handle as *mut std::ffi::c_void) });
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let req: Request = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let mut s = unsafe { std::fs::File::from_raw_handle(handle as _) };
                            let _ = writeln!(s, "{}", serde_json::to_string(&Response { ok: false, pane_id: None, panes: None, error: Some(format!("bad request: {e}")) }).unwrap());
                            return;
                        }
                    };
                    let cmd = match req.cmd.as_str() {
                        "split-pane" => Some(IpcCommand::SplitPane { args: req.args, dir: req.dir }),
                        "list" => Some(IpcCommand::ListPanes),
                        _ => None,
                    };
                    let Some(cmd) = cmd else {
                        let mut s = unsafe { std::fs::File::from_raw_handle(handle as _) };
                        let _ = writeln!(s, "{}", serde_json::to_string(&Response { ok: false, pane_id: None, panes: None, error: Some(format!("unknown cmd: {}", req.cmd)) }).unwrap());
                        return;
                    };
                    if cmd_tx.send(cmd).is_err() {
                        return;
                    }
                    let resp = match resp_rx.recv() {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    let mut s = unsafe { std::fs::File::from_raw_handle(handle as _) };
                    let _ = writeln!(s, "{}", serde_json::to_string(&resp).unwrap());
                    unsafe { DisconnectNamedPipe(handle as *mut std::ffi::c_void) };
                    unsafe { CloseHandle(handle as *mut std::ffi::c_void) };
                });
            }
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&path);
            let listener = match std::os::unix::net::UnixListener::bind(&path) {
                Ok(l) => l,
                Err(e) => {
                    log::error!("ipc: cannot bind {path:?}: {e}");
                    return;
                }
            };
            log::info!("ipc: listening on {path:?}");
            for stream in listener.incoming() {
                match stream {
                    Ok(s) => {
                        let cmd_tx = cmd_tx.clone();
                        let resp_rx = resp_rx.clone();
                        std::thread::spawn(move || {
                            let mut reader = BufReader::new(s.try_clone().ok());
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() {
                                return;
                            }
                            let req: Request = match serde_json::from_str(&line) {
                                Ok(r) => r,
                                Err(e) => {
                                    let mut s = s;
                                    let _ = writeln!(s, "{}", serde_json::to_string(&Response { ok: false, pane_id: None, panes: None, error: Some(format!("bad request: {e}")) }).unwrap());
                                    return;
                                }
                            };
                            let cmd = match req.cmd.as_str() {
                                "split-pane" => Some(IpcCommand::SplitPane { args: req.args, dir: req.dir }),
                                "list" => Some(IpcCommand::ListPanes),
                                _ => None,
                            };
                            let Some(cmd) = cmd else {
                                let mut s = s;
                                let _ = writeln!(s, "{}", serde_json::to_string(&Response { ok: false, pane_id: None, panes: None, error: Some(format!("unknown cmd: {}", req.cmd)) }).unwrap());
                                return;
                            };
                            if cmd_tx.send(cmd).is_err() {
                                return;
                            }
                            let resp = match resp_rx.recv() {
                                Ok(r) => r,
                                Err(_) => return,
                            };
                            let mut s = s;
                            let _ = writeln!(s, "{}", serde_json::to_string(&resp).unwrap());
                        });
                    }
                    Err(_) => break,
                }
            }
        }
    });

    IpcServer { cmd_rx, resp_tx }
}

/// Client: send a request and wait for the response.
pub fn send_request(req: &Request) -> Result<Response, String> {
    let path = ipc_path();
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let pipe_name: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                0,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(format!("cannot connect to termrs at {path:?} (is termrs running?)"));
        }
        let mut stream = unsafe { std::fs::File::from_raw_handle(handle as _) };
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        stream.write_all(b"\n").map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(unsafe { std::fs::File::from_raw_handle(handle as _) });
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).map_err(|e| e.to_string())?;
        serde_json::from_str(&resp_line).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let mut stream = std::os::unix::net::UnixStream::connect(&path)
            .map_err(|e| format!("cannot connect to termrs at {path:?}: {e} (is termrs running?)"))?;
        let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        stream.write_all(b"\n").map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).map_err(|e| e.to_string())?;
        serde_json::from_str(&resp_line).map_err(|e| e.to_string())
    }
}
