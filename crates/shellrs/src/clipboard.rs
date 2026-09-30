//! System clipboard access, with a clear failure message.
//!
//! Uses `arboard`; a headless or locked clipboard yields an `Err` the caller
//! surfaces as a status line rather than a panic.

/// Copy `text` to the system clipboard.
pub fn copy(text: &str) -> Result<(), String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .set_text(text.to_owned())
        .map_err(|e| format!("clipboard write failed: {e}"))
}

/// Read the system clipboard as text.
pub fn paste() -> Result<String, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| format!("clipboard unavailable: {e}"))?;
    clipboard
        .get_text()
        .map_err(|e| format!("clipboard read failed: {e}"))
}
