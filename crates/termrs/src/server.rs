//! HTTP server: remote control for termrs via REST API.
//!
//! Endpoints:
//!   POST /key       — send a key press to the focused pane
//!   POST /command   — type a full command string and press Enter
//!   POST /interrupt — send Ctrl+C (SIGINT) to the focused pane
//!   GET  /status    — get terminal text content
//!   GET  /screenshot — get PNG screenshot of the terminal

use crossbeam_channel::{Receiver, Sender};
use serde::Deserialize;
use tiny_http::{Header, Method, Request, Response, Server};

/// Key press request body.
#[derive(Debug, Clone, Deserialize)]
pub struct KeyRequest {
    pub key: String,
    #[serde(default)]
    pub modifier: Option<String>,
}

/// Command execution request body.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandRequest {
    pub command: String,
}

/// Commands the HTTP server sends to the app.
#[derive(Debug, Clone)]
pub enum ServerCommand {
    KeyPress { key: String, modifier: Option<String> },
    RunCommand(String),
    GetStatus,
    GetScreenshot,
    Quit,
}

/// Handle returned by the HTTP server.
pub struct HttpServerHandle {
    pub cmd_rx: Receiver<ServerCommand>,
    pub status_tx: Sender<String>,
    pub screenshot_tx: Sender<Vec<u8>>,
}

/// Start the HTTP server on a background thread.
pub fn start_server(addr: &str) -> Result<HttpServerHandle, String> {
    let server = Server::http(addr).map_err(|e| format!("cannot bind {addr}: {e}"))?;
    log::info!("http server listening on {addr}");

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ServerCommand>();
    let (status_tx, status_rx) = crossbeam_channel::unbounded::<String>();
    let (screenshot_tx, screenshot_rx) = crossbeam_channel::unbounded::<Vec<u8>>();

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let cmd_tx = cmd_tx.clone();
            let status_rx = status_rx.clone();
            let screenshot_rx = screenshot_rx.clone();
            std::thread::spawn(move || {
                handle_request(request, cmd_tx, status_rx, screenshot_rx);
            });
        }
    });

    Ok(HttpServerHandle {
        cmd_rx,
        status_tx,
        screenshot_tx,
    })
}

fn handle_request(
    mut request: Request,
    cmd_tx: Sender<ServerCommand>,
    status_rx: Receiver<String>,
    screenshot_rx: Receiver<Vec<u8>>,
) {
    let url = request.url().to_string();
    let method = request.method().clone();

    match (method, url.as_str()) {
        (Method::Post, "/key") => {
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let _ = request.respond(Response::from_string("bad request").with_status_code(400));
                return;
            }
            let key_req: KeyRequest = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(e) => {
                    let _ = request.respond(Response::from_string(format!("bad json: {e}")).with_status_code(400));
                    return;
                }
            };
            if cmd_tx.send(ServerCommand::KeyPress { key: key_req.key, modifier: key_req.modifier }).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            let _ = request.respond(Response::from_string("ok"));
        }
        (Method::Get, "/status") => {
            if cmd_tx.send(ServerCommand::GetStatus).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            match status_rx.recv() {
                Ok(text) => {
                    let _ = request.respond(Response::from_string(text));
                }
                Err(_) => {
                    let _ = request.respond(Response::from_string("timeout").with_status_code(500));
                }
            }
        }
        (Method::Post, "/interrupt") => {
            if cmd_tx.send(ServerCommand::KeyPress { key: "c".to_string(), modifier: Some("Ctrl".to_string()) }).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            let _ = request.respond(Response::from_string("ok"));
        }
        (Method::Post, "/command") => {
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                let _ = request.respond(Response::from_string("bad request").with_status_code(400));
                return;
            }
            let cmd_req: CommandRequest = match serde_json::from_str(&body) {
                Ok(r) => r,
                Err(e) => {
                    let _ = request.respond(Response::from_string(format!("bad json: {e}")).with_status_code(400));
                    return;
                }
            };
            if cmd_tx.send(ServerCommand::RunCommand(cmd_req.command)).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            let _ = request.respond(Response::from_string("ok"));
        }
        (Method::Post, "/quit") => {
            if cmd_tx.send(ServerCommand::Quit).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            let _ = request.respond(Response::from_string("ok"));
        }
        (Method::Get, "/screenshot") => {
            if cmd_tx.send(ServerCommand::GetScreenshot).is_err() {
                let _ = request.respond(Response::from_string("server error").with_status_code(500));
                return;
            }
            match screenshot_rx.recv() {
                Ok(png) => {
                    let header = Header::from_bytes(&b"Content-Type"[..], &b"image/png"[..]).unwrap();
                    let _ = request.respond(Response::from_data(png).with_header(header));
                }
                Err(_) => {
                    let _ = request.respond(Response::from_string("timeout").with_status_code(500));
                }
            }
        }
        _ => {
            let _ = request.respond(Response::from_string("not found").with_status_code(404));
        }
    }
}
