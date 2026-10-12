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
                            McpQueryKind::ScreenAnsi => {
                                McpReply::Text("SCREEN-\x1b[31mRED\x1b[0m".into())
                            }
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
                "clientInfo": { "name": "termrs-test", "version": "0.0" }
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
                "clientInfo": { "name": "termrs-test", "version": "0.0" }
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

        // tools/list -> all tools.
        let list = serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
        })
        .to_string();
        let (list_body, _) = post_rpc(&url, &list, Some(&session));
        for tool in [
            "termrs_run_command",
            "termrs_press_key",
            "termrs_mouse_click",
            "termrs_mouse_drag",
            "termrs_interrupt",
            "termrs_get_screen",
            "termrs_screenshot",
            "termrs_pane_info",
            "termrs_wait_screen",
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

        let screen = call(3, "termrs_get_screen", serde_json::json!({}));
        assert!(screen.contains("SCREEN-CONTENT"), "get_screen: {screen}");

        let run = call(
            4,
            "termrs_run_command",
            serde_json::json!({ "command": "echo hi" }),
        );
        assert!(run.contains("SCREEN-CONTENT"), "run_command: {run}");

        let key = call(5, "termrs_press_key", serde_json::json!({ "key": "Enter" }));
        assert!(key.contains("key sent"), "press_key: {key}");

        let click = call(
            9,
            "termrs_mouse_click",
            serde_json::json!({ "col": 3, "row": 2, "button": "right" }),
        );
        assert!(click.contains("clicked Right"), "mouse_click: {click}");

        let drag = call(
            10,
            "termrs_mouse_drag",
            serde_json::json!({ "col": 1, "row": 1, "to_col": 5, "to_row": 4 }),
        );
        assert!(drag.contains("dragged Left"), "mouse_drag: {drag}");

        let intr = call(6, "termrs_interrupt", serde_json::json!({}));
        assert!(intr.contains("interrupt sent"), "interrupt: {intr}");

        let shot = call(7, "termrs_screenshot", serde_json::json!({}));
        assert!(shot.contains("image/png"), "screenshot: {shot}");
        // base64 of the PNG magic bytes (mock is a 4-byte PNG).
        assert!(shot.contains("iVBORw"), "screenshot not base64 PNG: {shot}");

        let info = call(8, "termrs_pane_info", serde_json::json!({}));
        assert!(info.contains("pane_id"), "pane_info: {info}");

        stop.store(true, Ordering::Relaxed);
        let _ = responder.join();
        drop(server);
    }

    /// `termrs_wait_screen` times out with an error when the screen never
    /// changes (mock always returns the same text).
    #[test]
    fn wait_screen_times_out_when_unchanged() {
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

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "termrs-test", "version": "0.0" }
            }
        })
        .to_string();
        let (_, session) = post_rpc(&url, &init, None);
        let session = session.expect("initialize returned a session id");
        let ready = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string();
        let _ = post_rpc(&url, &ready, Some(&session));

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "termrs_wait_screen",
                "arguments": { "timeout_ms": 300, "interval_ms": 50 }
            }
        })
        .to_string();
        let (resp, _) = post_rpc(&url, &body, Some(&session));
        assert!(
            resp.contains("timed out") && resp.contains("screen unchanged"),
            "wait_screen timeout: {resp}"
        );

        stop.store(true, Ordering::Relaxed);
        let _ = responder.join();
        drop(server);
    }

    /// `termrs_wait_screen` returns the new text as soon as the screen differs.
    #[test]
    fn wait_screen_returns_on_change() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let server = start_pane_mcp(rt.handle(), tx, 7, 0).expect("server starts");
        let url = format!("http://127.0.0.1:{}/mcp", server.port);

        // Responder: first two screens identical, third differs.
        let responder = std::thread::spawn(move || {
            let mut n = 0;
            while let Ok(q) = rx.recv_timeout(Duration::from_millis(5000)) {
                let reply = match q.kind {
                    McpQueryKind::Screen | McpQueryKind::ScreenAnsi => {
                        n += 1;
                        if n < 3 {
                            McpReply::Text("BEFORE".into())
                        } else {
                            McpReply::Text("AFTER".into())
                        }
                    }
                    _ => McpReply::Ok,
                };
                let _ = q.reply.send(reply);
            }
        });

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "termrs-test", "version": "0.0" }
            }
        })
        .to_string();
        let (_, session) = post_rpc(&url, &init, None);
        let session = session.expect("initialize returned a session id");
        let ready = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string();
        let _ = post_rpc(&url, &ready, Some(&session));

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {
                "name": "termrs_wait_screen",
                "arguments": { "timeout_ms": 5000, "interval_ms": 50 }
            }
        })
        .to_string();
        let (resp, _) = post_rpc(&url, &body, Some(&session));
        assert!(resp.contains("AFTER"), "wait_screen change: {resp}");

        drop(server);
        let _ = responder.join();
    }

    #[test]
    fn wants_ansi_matches_loosely() {
        assert!(wants_ansi(Some("ansi")));
        assert!(wants_ansi(Some("ANSI")));
        assert!(wants_ansi(Some("  ansi  ")));
        assert!(!wants_ansi(None));
        assert!(!wants_ansi(Some("text")));
        assert!(!wants_ansi(Some("")));
    }

    /// `format: "ansi"` routes to `ScreenAnsi`; default stays plain.
    #[test]
    fn get_screen_format_selects_query() {
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

        let init = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "termrs-test", "version": "0.0" }
            }
        })
        .to_string();
        let (_, session) = post_rpc(&url, &init, None);
        let session = session.expect("initialize returned a session id");
        let ready = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        })
        .to_string();
        let _ = post_rpc(&url, &ready, Some(&session));

        let call = |id: i64, args: serde_json::Value| -> String {
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": { "name": "termrs_get_screen", "arguments": args }
            })
            .to_string();
            post_rpc(&url, &body, Some(&session)).0
        };

        let plain = call(3, serde_json::json!({}));
        assert!(plain.contains("SCREEN-CONTENT"), "plain default: {plain}");
        assert!(!plain.contains("u001b["), "plain has no SGR: {plain}");

        let ansi = call(4, serde_json::json!({ "format": "ansi" }));
        // JSON-RPC bodies escape ESC as \u001b, so match the escaped form.
        assert!(ansi.contains("u001b[31mRED"), "ansi SGR: {ansi}");

        stop.store(true, Ordering::Relaxed);
        let _ = responder.join();
        drop(server);
    }
