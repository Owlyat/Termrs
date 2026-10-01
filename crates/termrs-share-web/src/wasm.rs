//! wasm-bindgen wrapper around the iroh share client.

use std::str::FromStr;

use iroh::endpoint::presets;
use iroh::{Endpoint, EndpointAddr};
use iroh_tickets::endpoint::EndpointTicket;
use js_sys::{Function, Uint8Array};
use termrs_share_proto::{ALPN, Frame, Hello, Mode, LEN_PREFIX, body_len, decode_snapshot};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

/// Install panic logging once when the wasm module starts.
#[wasm_bindgen(start)]
fn start() {
    console_error_panic_hook::set_once();
}

fn to_js(err: impl std::fmt::Display) -> JsError {
    JsError::new(&err.to_string())
}

/// A live connection to a shared termrs pane.
#[wasm_bindgen]
pub struct ShareClient {
    input_tx: async_channel::Sender<Vec<u8>>,
    endpoint: Endpoint,
}

#[wasm_bindgen]
impl ShareClient {
    /// Connect to the host identified by `ticket`.
    ///
    /// `on_data` is called with terminal bytes (`Uint8Array`); `on_resize`
    /// with `(cols, rows)` before the first bytes; `on_close` when the
    /// connection ends. `code` is the session code shown by the host.
    #[wasm_bindgen]
    pub async fn connect(
        ticket: String,
        code: String,
        cols: u16,
        rows: u16,
        on_data: Function,
        on_resize: Function,
        on_close: Function,
    ) -> Result<ShareClient, JsError> {
        console_error_panic_hook::set_once();
        let endpoint = Endpoint::builder(presets::N0)
            .bind()
            .await
            .map_err(to_js)?;
        let addr: EndpointAddr = EndpointTicket::from_str(&ticket)
            .map_err(to_js)?
            .endpoint_addr()
            .clone();
        let conn = endpoint.connect(addr, ALPN).await.map_err(to_js)?;
        let (mut send, mut recv) = conn.open_bi().await.map_err(to_js)?;

        // Handshake: ask for control; the host grants it to the first viewer.
        let hello = Frame::Hello(Hello {
            mode: Mode::Control,
            cols,
            rows,
            code,
        });
        write_frame(&mut send, &hello).await.map_err(to_js)?;

        // Input pump: JS -> QUIC.
        let (input_tx, input_rx) = async_channel::bounded::<Vec<u8>>(256);
        spawn_local(async move {
            while let Ok(bytes) = input_rx.recv().await {
                if write_frame(&mut send, &Frame::Input(bytes))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        // Output pump: QUIC -> JS.
        spawn_local(async move {
            loop {
                match read_frame(&mut recv).await {
                    Ok(Frame::Snapshot(payload)) => {
                        if let Ok((cols, rows, ansi)) = decode_snapshot(&payload) {
                            let _ = on_resize.call2(
                                &JsValue::NULL,
                                &JsValue::from(cols),
                                &JsValue::from(rows),
                            );
                            let _ = on_data.call1(&JsValue::NULL, &Uint8Array::from(ansi).into());
                        }
                    }
                    Ok(Frame::Output(bytes)) => {
                        let _ = on_data
                            .call1(&JsValue::NULL, &Uint8Array::from(bytes.as_slice()).into());
                    }
                    Ok(Frame::Error(msg)) => {
                        tracing::warn!("share host error: {msg}");
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("share connection ended: {e}");
                        break;
                    }
                }
            }
            let _ = on_close.call0(&JsValue::NULL);
        });

        Ok(ShareClient { input_tx, endpoint })
    }

    /// Send keystrokes/paste to the host (ignored unless this client controls).
    #[wasm_bindgen]
    pub fn send(&self, bytes: &[u8]) {
        let _ = self.input_tx.try_send(bytes.to_vec());
    }

    /// Close the connection.
    #[wasm_bindgen]
    pub fn close(&self) {
        let endpoint = self.endpoint.clone();
        spawn_local(async move {
            endpoint.close().await;
        });
    }
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> Result<Frame, String> {
    use tokio::io::AsyncReadExt;
    let mut prefix = [0u8; LEN_PREFIX];
    reader
        .read_exact(&mut prefix)
        .await
        .map_err(|e| e.to_string())?;
    let len = body_len(prefix).map_err(|e| e.to_string())?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| e.to_string())?;
    Frame::decode(body[0], &body[1..]).map_err(|e| e.to_string())
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    writer
        .write_all(&frame.encode())
        .await
        .map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())
}
