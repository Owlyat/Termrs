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
    Key {
        key: String,
        modifier: Option<String>,
        times: u32,
    },
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
    ScreenAnsi,
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
                name: "termrs-pane".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some(format!("termrs MCP (pane {pane_id})")),
                description: Some(
                    "Drive a single termrs terminal pane: run commands, press keys, \
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
                "This server controls ONE terminal pane of the termrs app. Use \
                 termrs_run_command to execute shell commands and read their output, \
                 termrs_press_key, termrs_mouse_click and termrs_mouse_drag to drive \
                 interactive programs, termrs_get_screen to inspect the current terminal, \
                 termrs_wait_screen to hang until the screen changes, and \
                 termrs_screenshot for an image of the pane. There is deliberately no \
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
                match screen_text(&self.shared, wants_ansi(t.format.as_deref())).await {
                    Ok(text) => Ok(CallToolResult::text_content(vec![text_block(text)])),
                    Err(e) => Err(e),
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
            PaneTools::WaitScreenTool(t) => wait_screen(&self.shared, t).await,
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

/// `"ansi"` (any case, surrounding whitespace ignored) requests SGR-colored
/// screen text; anything else (including absent) means plain text.
fn wants_ansi(format: Option<&str>) -> bool {
    format.is_some_and(|f| f.trim().eq_ignore_ascii_case("ansi"))
}

/// Ask the UI thread for the screen in plain or ANSI form.
async fn screen_text(shared: &McpShared, ansi: bool) -> Result<String, CallToolError> {
    let kind = if ansi {
        McpQueryKind::ScreenAnsi
    } else {
        McpQueryKind::Screen
    };
    match ask(shared, kind).await? {
        McpReply::Text(text) => Ok(text),
        McpReply::Err(e) => Err(CallToolError::from_message(e)),
        _ => Err(CallToolError::from_message("pane returned no text")),
    }
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
    let ansi = wants_ansi(tool.format.as_deref());
    match screen_text(shared, ansi).await {
        Ok(text) => Ok(CallToolResult::text_content(vec![TextContent::from(text)])),
        Err(e) => Err(e),
    }
}

async fn press_key(
    shared: &McpShared,
    tool: PressKeyTool,
) -> Result<CallToolResult, CallToolError> {
    let times = tool.times.unwrap_or(1).clamp(1, 100);
    let ok = if times == 1 {
        "key sent".to_string()
    } else {
        format!("key sent x{times}")
    };
    simple(
        shared,
        McpQueryKind::Key {
            key: tool.key,
            modifier: tool.modifier,
            times,
        },
        &ok,
    )
    .await
}

/// Capture the screen, then hang until it differs (or timeout).
/// Polls on the MCP task so the UI thread stays responsive.
async fn wait_screen(
    shared: &McpShared,
    tool: WaitScreenTool,
) -> Result<CallToolResult, CallToolError> {
    let timeout = Duration::from_millis(tool.timeout_ms.unwrap_or(15_000).clamp(100, 120_000));
    let interval = Duration::from_millis(tool.interval_ms.unwrap_or(250).clamp(50, 5_000));
    let ansi = wants_ansi(tool.format.as_deref());
    let before = match screen_text(shared, ansi).await {
        Ok(text) => text,
        Err(e) => return Err(e),
    };
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= timeout {
            return Err(CallToolError::from_message(format!(
                "timed out after {}ms, screen unchanged",
                timeout.as_millis()
            )));
        }
        tokio::time::sleep(interval).await;
        match screen_text(shared, ansi).await {
            Ok(now) if now != before => {
                return Ok(CallToolResult::text_content(vec![TextContent::from(now)]));
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
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
    name = "termrs_run_command",
    title = "Run a shell command in the pane",
    description = "Type a command into this pane's shell and press Enter, then return \
                   the terminal contents after a short delay. Set `format` to \"ansi\" \
                   for ANSI SGR color escapes, otherwise plain text.",
    destructive_hint = true,
    open_world_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct RunCommandTool {
    /// The full command line to run (without a trailing newline).
    pub command: String,
    /// `"ansi"` for SGR-colored output, anything else (or omitted) for plain text.
    #[serde(default)]
    pub format: Option<String>,
}

#[mcp_tool(
    name = "termrs_press_key",
    title = "Press a key in the pane",
    description = "Send a single key press to the pane, e.g. for interactive programs. \
                   `key` is one of Enter, Escape, Backspace, Tab, Up, Down, Left, Right, \
                   Home, End, PageUp, PageDown, Delete, Insert, F1..F12, or one character. \
                   `times` repeats the press (1-100, default 1), e.g. Down x4.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct PressKeyTool {
    /// Key name or single character.
    pub key: String,
    /// Optional modifier containing any of Ctrl, Alt, Shift.
    #[serde(default)]
    pub modifier: Option<String>,
    /// How many times to press the key (1-100, default 1).
    #[serde(default)]
    pub times: Option<u32>,
}

#[mcp_tool(
    name = "termrs_mouse_click",
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
    name = "termrs_mouse_drag",
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
    name = "termrs_interrupt",
    title = "Interrupt the foreground process",
    description = "Send Ctrl+C (interrupt) to the pane, stopping whatever is running.",
    destructive_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct InterruptTool {}

#[mcp_tool(
    name = "termrs_get_screen",
    title = "Read the terminal screen",
    description = "Return the current visible text of the pane as plain text, or with \
                   ANSI SGR color escapes when `format` is \"ansi\".",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct GetScreenTool {
    /// `"ansi"` for SGR-colored output, anything else (or omitted) for plain text.
    #[serde(default)]
    pub format: Option<String>,
}

#[mcp_tool(
    name = "termrs_screenshot",
    title = "Screenshot the pane",
    description = "Return a PNG image of the pane's current contents.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct ScreenshotTool {}

#[mcp_tool(
    name = "termrs_pane_info",
    title = "Describe the pane",
    description = "Return JSON with the pane id, title, working directory, shell and \
                   liveness. Never includes a way to quit the application.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema, Default)]
pub struct PaneInfoTool {}

#[mcp_tool(
    name = "termrs_wait_screen",
    title = "Wait for the screen to change",
    description = "Capture the current screen text, then hang until it differs and return \
                   the new text. Times out with an error if nothing changes in `timeout_ms` \
                   (default 15000, max 120000). Polls every `interval_ms` (default 250). \
                   Set `format` to \"ansi\" for ANSI SGR color escapes.",
    read_only_hint = true
)]
#[derive(Debug, serde::Deserialize, serde::Serialize, JsonSchema)]
pub struct WaitScreenTool {
    /// Max milliseconds to wait (100-120000, default 15000).
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Poll interval in milliseconds (50-5000, default 250).
    #[serde(default)]
    pub interval_ms: Option<u64>,
    /// `"ansi"` for SGR-colored output, anything else (or omitted) for plain text.
    #[serde(default)]
    pub format: Option<String>,
}

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
        PaneInfoTool,
        WaitScreenTool
    ]
);

#[cfg(test)]
#[path = "test/mcp_tests.rs"]
mod tests;
