//! Image decoding shared by the backdrop and the `ctrl+i` overlay.
//!
//! Shellrs is the terminal, so it draws images itself via a wgpu post-process
//! pass (see `image_gpu.rs`) rather than emitting sixel/kitty sequences for an
//! outer terminal to interpret.

/// Decoded image: tightly packed RGBA8 pixels plus dimensions.
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Decode an image file to RGBA.
pub fn decode(path: &str) -> Result<Rgba, String> {
    let img = image::open(path).map_err(|e| format!("{path}: {e}"))?;
    let rgba = img.to_rgba8();
    Ok(Rgba {
        width: rgba.width(),
        height: rgba.height(),
        data: rgba.into_raw(),
    })
}

/// One open overlay image: decoded RGBA bytes plus dimensions.
pub struct ImageView {
    /// File path as given (shown in the title).
    pub path: String,
    /// Pixel dimensions of the decoded image.
    pub dims: (u32, u32),
    /// Tightly packed RGBA8 pixels (`dims.0 * dims.1 * 4` bytes).
    pub rgba: Vec<u8>,
}

impl ImageView {
    /// Decode `path` into RGBA.
    pub fn open(path: &str) -> Result<Self, String> {
        let rgba = decode(path)?;
        Ok(Self {
            path: path.to_string(),
            dims: (rgba.width, rgba.height),
            rgba: rgba.data,
        })
    }
}

/// Whether `name` has a raster-image extension we can decode.
pub fn is_image(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["png", "jpg", "jpeg", "gif", "bmp", "webp"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}
