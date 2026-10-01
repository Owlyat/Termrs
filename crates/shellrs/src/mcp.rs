//! Pane-scoped MCP server.
//!
//! `Open MCP pane` (command palette) splits a normal shell pane and attaches
//! a [rust-mcp-sdk](https://github.com/rust-mcp-stack/rust-mcp-sdk) Streamable
//! HTTP MCP server to *that pane only*. An AI client can run commands, press
//! keys, read the screen and take a screenshot of the one pane; there is no
//! tool to quit the app. Closing the pane shuts the server down.
//!
//! The MCP server runs on the window's tokio runtime, but every tool call has
//! to touch pane state owned by the UI thread (the `vt100` screen and the PTY
//! writer are not `Sync`). Tool calls therefore send a [`McpQuery`] over a
//! crossbeam channel and await a `tokio::sync::oneshot` reply that the UI
//! thread produces in [`crate::app::App::poll_mcp`].

// The MCP tool structs are named `...Tool` by the SDK's convention, which the
// generated `tool_box!` enum mirrors; the shared suffix is intentional.
#![allow(clippy::enum_variant_names)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use rust_mcp_axum::{create_axum_server, AxumServerOptions};
use rust_mcp_sdk::macros::{mcp_tool, JsonSchema};
use rust_mcp_sdk::mcp_server::{ServerHandler, ToMcpServerHandler};
use rust_mcp_sdk::schema::schema_utils::CallToolError;
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ImageContent, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ProtocolVersion, RpcError, ServerCapabilities,
    ServerCapabilitiesTools, TextContent,
};
use rust_mcp_sdk::{tool_box, McpServer};

/// A request from an MCP tool to the UI thread.
pub struct McpQuery {
    pub pane_id: usize,
    pub kind: McpQueryKind,
    /// UI thread fills this in; the tool call awaits it.
    pub reply: tokio::sync::oneshot::Sender<McpReply>,
}

/// What the tool wants the pane to do.
pub enum McpQueryKind {
    Run(String),
    Key { key: String, modifier: Option<String> },
    Mouse {
        col: u16,
        row: u16,
        button: crate::mouse::MouseButton,
        modifier: Option<String>,
    },
    Drag {
        from: (u16, u16),
        to: (u16, u16),
        button: crate::mouse::MouseButton,
        modifier: Option<String>,
    },
    Interrupt,
    Screen,
    Screenshot,
    Info,
}

/// The UI thread's answer.
pub enum McpReply {
    Ok,
    Text(String),
    Png(Vec<u8>),
    Err(String),
}

/// Build a `TextContent` for a tool result.
fn text_block(value: String) -> TextContent {
    TextContent::new(value, None, None)
}

/// State shared between the UI thread and the running MCP server, including
/// the bit shown at the top of the pane.
pub struct McpShared {
    pub pane_id: usize,
    pub port: u16,
    pub query_tx: crossbeam_channel::Sender<McpQuery>,
    pub requests: AtomicU64,
    pub last_tool: Mutex<String>,
}

impl McpShared {
    /// Compact status line rendered in the pane title.
    pub fn status(&self) -> String {
        let n = self.requests.load(Ordering::Relaxed);
        let last = self
            .last_tool
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        if last.is_empty() {
            format!("MCP :{} · {n} req", self.port)
        } else {
            format!("MCP :{} · {n} req · {last}", self.port)
        }
    }
}

/// A running MCP server owned by one pane.
pub struct PaneMcp {
    pub port: u16,
    pub shared: Arc<McpShared>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PaneMcp {
    /// Ask the server to stop (graceful write, bounded timeout). Idempotent.
    pub fn stop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Ask the OS for a free localhost port.
fn free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("no free port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| e.to_string())?
        .port();
    drop(listener);
    Ok(port)
}

/// Resolve the port to serve on: `0` picks a free one, otherwise the fixed
/// port must be available (so a hardcoded client URL stays valid).
fn resolve_port(base_port: u16) -> Result<u16, String> {
    if base_port == 0 {
        return free_port();
    }
    match std::net::TcpListener::bind(("127.0.0.1", base_port)) {
        Ok(listener) => {
            drop(listener);
            Ok(base_port)
        }
        Err(e) => Err(format!("MCP port {base_port} is unavailable: {e}")),
    }
}

/// Start a pane-scoped MCP server. `query_tx` is the UI thread's query inbox.
pub fn start_pane_mcp(
    rt: &tokio::runtime::Handle,
    query_tx: crossbeam_channel::Sender<McpQuery>,
    pane_id: usize,
    base_port: u16,
) -> Result<PaneMcp, String> {
    let port = resolve_port(base_port)?;

    let shared = Arc::new(McpShared {
        pane_id,
        port,
        query_tx,
        requests: AtomicU64::new(0),
        last_tool: Mutex::new(String::new()),
    });

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let handler_shared = Arc::clone(&shared);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    rt.spawn(async move {
        let details = InitializeResult {
            server_info: Implementation {
                name: "shellrs-pane".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some(format!("shellrs MCP (pane {pane_id})")),
                description: Some(
                    "Drive a single shellrs terminal pane: run commands, press keys, \
                     read the screen. Closing the pane stops this server."
                        .into(),
                ),
                icons: vec![],
                website_url: None,
            },
            capabilities: ServerCapabilities {
                tools: Some(ServerCapabilitiesTools { list_changed: None }),
                ..Default::default()
            },
            instructions: Some(
                "This server controls ONE terminal pane of the shellrs app. Use \
                 shellrs_run_command to execute shell commands and read their output, \
                 shellrs_press_key, shellrs_mouse_click and shellrs_mouse_drag to drive \
                 interactive programs, shellrs_get_screen to inspect the current terminal, and \
                 shellrs_screenshot for an image of the pane. There is deliberately no \
                 tool to close the app."
                    .into(),
            ),
            meta: None,
            protocol_version: ProtocolVersion::V2025_11_25.into(),
        };

        let server = create_axum_server(
            details,
            PaneMcpHandler {
                shared: handler_shared,
            }
            .to_mcp_server_handler(),
            AxumServerOptions {
                host: "127.0.0.1".into(),
                port,
                ..Default::default()
            },
        );

        match server.start_runtime().await {
            Ok(runtime) => {
                let _ = ready_tx.send(Ok(()));
                // Block this task until the pane closes, then drain in-flight
                // requests before the task ends.
                let _ = stop_rx.await;
                runtime.graceful_shutdown(Some(Duration::from_secs(2)));
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e.to_string()));
            }
        }
    });

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(PaneMcp {
            port,
            shared,
            stop_tx: Some(stop_tx),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("MCP server did not start in time".into()),
    }
}

//***************//
//  MCP handler  //
//***************//

struct PaneMcpHandler {
    shared: Arc<McpShared>,
}

#[async_trait::async_trait]
impl ServerHandler for PaneMcpHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: PaneTools::tools(),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        self.shared.requests.fetch_add(1, Ordering::Relaxed);
        let tool = PaneTools::try_from(params).map_err(CallToolError::new)?;
        if let Ok(mut name) = self.shared.last_tool.lock() {
            *name = tool.tool_name();
        }

        match tool {
            PaneTools::RunCommandTool(t) => run_command(&self.shared, t).await,
            PaneTools::PressKeyTool(t) => press_key(&self.shared, t).await,
            PaneTools::MouseClickTool(t) => mouse_click(&self.shared, t).await,
            PaneTools::MouseDragTool(t) => mouse_drag(&self.shared, t).await,
            PaneTools::InterruptTool(t) => {
                let _ = t;
                simple(&self.shared, McpQueryKind::Interrupt, "interrupt sent").await
            }
            PaneTools::GetScreenTool(t) => {
                let _ = t;
                match ask(&self.shared, McpQueryKind::Screen).await? {
                    McpReply::Text(text) => {
                        Ok(CallToolResult::text_content(vec![text_block(text)]))
                    }
                    _ => Err(CallToolError::from_message("pane returned no text")),
                }
            }
            PaneTools::ScreenshotTool(t) => {
                let _ = t;
                match ask(&self.shared, McpQueryKind::Screenshot).await? {
                    McpReply::Png(png) => {
                        let data = base64::engine::general_purpose::STANDARD.encode(&png);
                        Ok(CallToolResult::image_content(vec![ImageContent::new(
                            data,
                            "image/png".to_string(),
                            None,
                            None,
                        )]))
                    }
                    _ => Err(CallToolError::from_message("pane returned no image")),
                }
            }
            PaneTools::PaneInfoTool(t) => {
                let _ = t;
                match ask(&self.shared, McpQueryKind::Info).await? {
                    McpReply::Text(text) => {
                        Ok(CallToolResult::text_content(vec![text_block(text)]))
                    }
                    _ => Err(CallToolError::from_message("pane returned no info")),
                }
            }
        }
    }
}

/// Send one query and await the UI thread's reply.
async fn ask(shared: &McpShared, kind: McpQueryKind) -> Result<McpReply, CallToolError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    shared
        .query_tx
        .send(McpQuery {
            pane_id: shared.pane_id,
            kind,
            reply: tx,
        })
        .map_err(|e| CallToolError::from_message(format!("pane is gone: {e}")))?;
    rx.await
        .map_err(|e| CallToolError::from_message(format!("pane did not answer: {e}")))
}

/// Fire-and-forget query that maps a success reply to a short text result.
async fn simple(
    shared: &McpShared,
    kind: McpQueryKind,
    ok_text: &str,
) -> Result<CallToolResult, CallToolError> {
    match ask(shared, kind).await? {
        McpReply::Err(e) => Err(CallToolError::from_message(e)),
        _ => Ok(CallToolResult::text_content(vec![text_block(
            ok_text.to_string(),
        )])),
    }
}

async fn run_command(
    shared: &McpShared,
    tool: RunCommandTool,
) -> Result<CallToolResult, CallToolError> {
    if let McpReply::Err(e) = ask(shared, McpQueryKind::Run(tool.command)).await? {
        return Err(CallToolError::from_message(e));
    }
    // Give the shell a moment to produce output, then return what is on screen.
    tokio::time::sleep(Duration::from_millis(250)).await;
    match ask(shared, McpQueryKind::Screen).await {
        Ok(McpReply::Text(text)) => Ok(CallToolResult::text_content(vec![TextContent::from(text)])),
        Ok(_) => Ok(CallToolResult::text_content(vec![text_block(
            "command sent".to_string(),
        )])),
        Err(e) => Err(e),
    }
}

async fn press_key(
    shared: &McpShared,
    tool: PressKeyTool,
) -> Result<CallToolResult, CallToolError> {
    simple(
        shared,
        McpQueryKind::Key {
            key: tool.key,
            modifier: tool.modifier,
        },
        "key sent",
    )
    .await
}

/// Parse a tool's `button` argument (defaults to left).
fn mouse_button_arg(button: Option<&str>) -> Result<crate::mouse::MouseButton, CallToolError> {
    let named = button.unwrap_or("left").trim().to_ascii_lowercase();
    match named.as_str() {
        "left" | "" => Ok(crate::mouse::MouseButton::Left),
        "middle" => Ok(crate::mouse::MouseButton::Middle),
        "right" => Ok(crate::mouse::MouseButton::Right),
        other => Err(CallToolError::from_message(format!(
            "unknown button {other:?} (use left, middle or right)"
        ))),
    }
}

/// Await a query whose success reply is a short status string.
async fn mouse_result(
    shared: &McpShared,
    kind: McpQueryKind,
) -> Result<CallToolResult, CallToolError> {
    match ask(shared, kind).await? {
        McpReply::Text(text) => Ok(CallToolResult::text_content(vec![text_block(text)])),
        McpReply::Err(e) => Err(CallToolError::from_message(e)),
        _ => Err(CallToolError::from_message("pane returned no status")),
    }
}

async fn mouse_click(
    shared: &McpShared,
    tool: MouseClickTool,
) -> Result<CallToolResult, CallToolError> {
    let button = mouse_button_arg(tool.button.as_deref())?;
    mouse_result(
        shared,
        McpQueryKind::Mouse {
            col: tool.col,
            row: tool.row,
            button,
            modifier: tool.modifier,
        },
    )
    .await
}

async fn mouse_drag(
    shared: &McpShared,
    tool: MouseDragTool,
) -> Result<CallToolResult, CallToolError> {
    let button = mouse_button_arg(tool.button.as_deref())?;
    mouse_result(
        shared,
        McpQueryKind::Drag {
            from: (tool.col, tool.row),
            to: (tool.to_col, tool.to_row),
            button,
            modifier: tool.modifier,
        },
    )
    .await
}

//***********//
//  Tools    //
//***********//

#[mcp_tool(
    name = "shellrs_run_command",
    title = "Run a shell command in the pane",
    description = "Type a command into this pane's shell and press Enter, then return \
                   the terminal contents after a short delay.",
    destructive_hint = true,
    open_world_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct RunCommandTool {
    /// The full command line to run (without a trailing newline).
    pub command: String,
}

#[mcp_tool(
    name = "shellrs_press_key",
    title = "Press a key in the pane",
    description = "Send a single key press to the pane, e.g. for interactive programs. \
                   `key` is one of Enter, Escape, Backspace, Tab, Up, Down, Left, Right, \
                   Home, End, PageUp, PageDown, Delete, Insert, F1..F12, or one character.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct PressKeyTool {
    /// Key name or single character.
    pub key: String,
    /// Optional modifier containing any of Ctrl, Alt, Shift.
    #[serde(default)]
    pub modifier: Option<String>,
}

#[mcp_tool(
    name = "shellrs_mouse_click",
    title = "Click in the pane",
    description = "Send a mouse click (press then release) to a cell of this pane, for \
                   interactive apps that enable mouse tracking. Coordinates are zero-based \
                   cells within the pane's text grid: `col` from the left edge, `row` from \
                   the top edge. `button` is left (default), middle or right, and `modifier` \
                   may contain Ctrl, Alt, Shift.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct MouseClickTool {
    /// Zero-based column, from the pane's left edge.
    pub col: u16,
    /// Zero-based row, from the pane's top edge.
    pub row: u16,
    /// Mouse button: left (default), middle or right.
    #[serde(default)]
    pub button: Option<String>,
    /// Optional modifier containing any of Ctrl, Alt, Shift.
    #[serde(default)]
    pub modifier: Option<String>,
}

#[mcp_tool(
    name = "shellrs_mouse_drag",
    title = "Drag in the pane",
    description = "Press `button` at a start cell, move the pointer to an end cell, then \
                   release, for interactive apps that enable mouse tracking. Coordinates are \
                   zero-based cells within the pane's text grid: (`col`, `row`) is the start \
                   and (`to_col`, `to_row`) the end. `button` is left (default), middle or \
                   right, and `modifier` may contain Ctrl, Alt, Shift.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct MouseDragTool {
    /// Zero-based start column, from the pane's left edge.
    pub col: u16,
    /// Zero-based start row, from the pane's top edge.
    pub row: u16,
    /// Zero-based end column.
    pub to_col: u16,
    /// Zero-based end row.
    pub to_row: u16,
    /// Mouse button: left (default), middle or right.
    #[serde(default)]
    pub button: Option<String>,
    /// Optional modifier containing any of Ctrl, Alt, Shift.
    #[serde(default)]
    pub modifier: Option<String>,
}

#[mcp_tool(
    name = "shellrs_interrupt",
    title = "Interrupt the foreground process",
    description = "Send Ctrl+C (interrupt) to the pane, stopping whatever is running.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct InterruptTool {}

#[mcp_tool(
    name = "shellrs_get_screen",
    title = "Read the terminal screen",
    description = "Return the current visible text of the pane as plain text.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct GetScreenTool {}

#[mcp_tool(
    name = "shellrs_screenshot",
    title = "Screenshot the pane",
    description = "Return a PNG image of the pane's current contents.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct ScreenshotTool {}

#[mcp_tool(
    name = "shellrs_pane_info",
    title = "Describe the pane",
    description = "Return JSON with the pane id, title, working directory, shell and \
                   liveness. Never includes a way to quit the application.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct PaneInfoTool {}

// The generated enum mirrors the tool structs, so every variant shares the
// `Tool` suffix; that is intentional here (allowed at module level).
tool_box!(
    PaneTools,
    [
        RunCommandTool,
        PressKeyTool,
        MouseClickTool,
        MouseDragTool,
        InterruptTool,
        GetScreenTool,
        ScreenshotTool,
        PaneInfoTool
    ]
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// POST a JSON-RPC body, returning `(response body, session id)`. The
    /// session id from `initialize` must be echoed on later requests.
    fn post_rpc(url: &str, body: &str, session: Option<&str>) -> (String, Option<String>) {
        let mut req = ureq::post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .set("MCP-Protocol-Version", "2025-11-25");
        if let Some(s) = session {
            req = req.set("Mcp-Session-Id", s);
        }
        let resp = req.send_string(body).expect("request had a response");
        let sid = resp.header("mcp-session-id").map(str::to_string);
        (resp.into_string().unwrap_or_default(), sid)
    }

    /// Spawn a stand-in for the UI thread that answers every pane query with
    /// canned data, so the MCP surface can be driven without a real pane.
    fn spawn_responder(
        rx: crossbeam_channel::Receiver<McpQuery>,
        stop: std::sync::Arc<AtomicBool>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(q) => {
                        let reply = match q.kind {
                            McpQueryKind::Run(_) | McpQueryKind::Key { .. }
                            | McpQueryKind::Interrupt => McpReply::Ok,
                            McpQueryKind::Mouse { button, .. } => {
                                McpReply::Text(format!("clicked {button:?}"))
                            }
                            McpQueryKind::Drag { button, .. } => {
                                McpReply::Text(format!("dragged {button:?}"))
                            }
                            McpQueryKind::Screen => McpReply::Text("SCREEN-CONTENT".into()),
                            McpQueryKind::Screenshot => {
                                McpReply::Png(vec![0x89, 0x50, 0x4E, 0x47])
                            }
                            McpQueryKind::Info => McpReply::Text("{\"pane_id\":7}".into()),
                        };
                        let _ = q.reply.send(reply);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
    }

    /// The server must answer the standard MCP `initialize` handshake over
    /// Streamable HTTP: that is what external clients (opencode, MCP
    /// Inspector) do first, and the 2.x stateless protocol rejected it.
    #[test]
    fn server_answers_initialize_handshake() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, _rx) = crossbeam_channel::unbounded();
        // Port 0 = pick a free one; PaneMcp reports back which it got.
        let server = start_pane_mcp(rt.handle(), tx, 1, 0).expect("server starts");

        let url = format!("http://127.0.0.1:{}/mcp", server.port);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "shellrs-test", "version": "0.0" }
            }
        })
        .to_string();

        let (text, _session) = post_rpc(&url, &body, None);
        assert!(
            text.contains("serverInfo") && text.contains("protocolVersion"),
            "initialize handshake failed: {text}"
        );

        drop(server);
    }

    /// Full surface: initialize, list tools, then call every tool and check
    /// the canned pane reply comes back through the MCP result.
    #[test]
    fn server_serves_all_tools() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let server = start_pane_mcp(rt.handle(), tx, 7, 0).expect("server starts");
        let url = format!("http://127.0.0.1:{}/mcp", server.port);

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let responder = spawn_responder(rx, stop.clone());

        // initialize -> session id
        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "shellrs-test", "version": "0.0" }
            }
        })
        .to_string();
        let (init_body, session) = post_rpc(&url, &init, None);
        assert!(init_body.contains("serverInfo"), "initialize: {init_body}");
        let session = session.expect("initialize returned a session id");

        // Complete the handshake (notification, no id).
        let ready = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string();
        let _ = post_rpc(&url, &ready, Some(&session));

        // tools/list -> all six tools.
        let list = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        })
        .to_string();
        let (list_body, _) = post_rpc(&url, &list, Some(&session));
        for tool in [
            "shellrs_run_command",
            "shellrs_press_key",
            "shellrs_mouse_click",
            "shellrs_mouse_drag",
            "shellrs_interrupt",
            "shellrs_get_screen",
            "shellrs_screenshot",
            "shellrs_pane_info",
        ] {
            assert!(list_body.contains(tool), "tools/list missing {tool}: {list_body}");
        }

        // tools/call for each tool; check the mock reply round-trips.
        let call = |id: i64, name: &str, args: serde_json::Value| -> String {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": name, "arguments": args }
            })
            .to_string();
            post_rpc(&url, &body, Some(&session)).0
        };

        let screen = call(3, "shellrs_get_screen", serde_json::json!({}));
        assert!(screen.contains("SCREEN-CONTENT"), "get_screen: {screen}");

        let run = call(
            4,
            "shellrs_run_command",
            serde_json::json!({ "command": "echo hi" }),
        );
        assert!(run.contains("SCREEN-CONTENT"), "run_command: {run}");

        let key = call(5, "shellrs_press_key", serde_json::json!({ "key": "Enter" }));
        assert!(key.contains("key sent"), "press_key: {key}");

        let click = call(
            9,
            "shellrs_mouse_click",
            serde_json::json!({ "col": 3, "row": 2, "button": "right" }),
        );
        assert!(click.contains("clicked Right"), "mouse_click: {click}");

        let drag = call(
            10,
            "shellrs_mouse_drag",
            serde_json::json!({ "col": 1, "row": 1, "to_col": 5, "to_row": 4 }),
        );
        assert!(drag.contains("dragged Left"), "mouse_drag: {drag}");

        let intr = call(6, "shellrs_interrupt", serde_json::json!({}));
        assert!(intr.contains("interrupt sent"), "interrupt: {intr}");

        let shot = call(7, "shellrs_screenshot", serde_json::json!({}));
        assert!(shot.contains("image/png"), "screenshot: {shot}");
        // base64 of the PNG magic bytes (mock is a 4-byte PNG).
        assert!(shot.contains("iVBORw"), "screenshot not base64 PNG: {shot}");

        let info = call(8, "shellrs_pane_info", serde_json::json!({}));
        assert!(info.contains("pane_id"), "pane_info: {info}");

        stop.store(true, Ordering::Relaxed);
        let _ = responder.join();
        drop(server);
    }
}



