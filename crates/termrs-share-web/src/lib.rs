//! Browser client for termrs terminal sharing, compiled to WebAssembly.
//!
//! Pairs with the termrs `Share terminal` command: the [`ShareClient`] dials
//! the host's iroh endpoint over a relay, receives the pane's screen and live
//! output, and forwards keystrokes back. The web page (`web/`) wires this to
//! xterm.js.
//!
//! Everything here is wasm-only; on a native target the crate builds empty so
//! it can stay a workspace member without pulling browser code into the app.

#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(target_arch = "wasm32")]
pub use wasm::ShareClient;
