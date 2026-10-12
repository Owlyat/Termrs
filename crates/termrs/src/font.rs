//! Monospace font discovery for the native window renderer.
//!
//! The GPU backend needs raw font bytes that outlive it, so the chosen file is
//! read once and leaked to `&'static [u8]`. A path in `[general] font`
//! overrides auto-detection; `[general] font_fallback` adds glyph-fallback
//! fonts (Nerd Font icons, symbols) that are tried for characters the primary
//! font lacks.

/// Primary font plus ordered fallbacks, all leaked to `'static`.
pub struct FontSet {
    pub primary: &'static [u8],
    pub fallbacks: Vec<&'static [u8]>,
}

/// Read the configured font (or the first system monospace found) and any
/// fallback fonts. Extended glyphs (Nerd Font icons, symbols, braille, CJK)
/// come from the fallbacks.
pub fn load_set(configured: &str, fallbacks: &[String]) -> Result<FontSet, String> {
    let primary = if configured.trim().is_empty() {
        let mut found = None;
        for path in candidates() {
            if let Ok(bytes) = std::fs::read(&path) {
                found = Some(Box::leak(bytes.into_boxed_slice()) as &'static [u8]);
                break;
            }
        }
        found.ok_or("no monospace font found; set `font = \"<path to .ttf>\"` in [general]")?
    } else {
        let bytes = std::fs::read(configured)
            .map_err(|e| format!("cannot read font {configured:?}: {e}"))?;
        Box::leak(bytes.into_boxed_slice())
    };

    let mut set = Vec::new();
    if fallbacks.is_empty() {
        // Auto: every readable fallback candidate, monospace first. A single
        // fallback leaves whole scripts as tofu (braille lives in Segoe UI
        // Symbol, emoji in Segoe UI Emoji, ...). Proportional files still
        // load (coverage matters more than advances for rare glyphs) but sort
        // last so common text never touches their advances.
        for mut bytes in collect_auto_fallbacks(&fallback_candidates()) {
            normalize_fallback_metrics(primary, &mut bytes);
            set.push(Box::leak(bytes.into_boxed_slice()) as &'static [u8]);
        }
    } else {
        for f in fallbacks {
            match std::fs::read(f) {
                Ok(mut bytes) => {
                    normalize_fallback_metrics(primary, &mut bytes);
                    set.push(Box::leak(bytes.into_boxed_slice()) as &'static [u8]);
                }
                Err(e) => eprintln!("termrs: cannot read fallback font {f:?}: {e}"),
            }
        }
    }
    Ok(FontSet {
        primary,
        fallbacks: set,
    })
}

/// Widen a fallback's line metrics so it cannot shrink the terminal cell.
///
/// `ratatui-wgpu` derives the cell width as `advance('m') * size / height` and
/// takes the **minimum over every font**, including fallbacks. A narrow Nerd
/// Font fallback (JetBrains Mono at 0.6 em advance vs Cascadia's 0.504) then
/// shrinks every cell and stretches the grid vertically, which makes braille
/// dots (a fixed 2x4 sub-grid) visibly non-square -- and mismatches WezTerm,
/// whose cell width comes from the primary font alone.
///
/// Only the fallback's `ascender - descender` is rescaled, never advances.
/// `ratatui-wgpu` normalises each glyph's width to the cell, so a fallback
/// glyph's rendered size depends only on its advance, not on `height()`:
/// shrinking the height raises `char_width()` to match the primary while the
/// drawn glyphs stay the same size (and the ascender/descender scale together,
/// so the baseline does not move).
fn normalize_fallback_metrics(primary: &[u8], fallback: &mut [u8]) {
    let Some(r) = advance_ratio(primary) else {
        return;
    };
    let Some(fr) = advance_ratio(fallback) else {
        return;
    };
    if fr.0 / fr.1 >= r.0 / r.1 {
        return;
    }
    // Target height that makes this font's cell width match the primary.
    let target = (fr.0 / (r.0 / r.1)).floor() - 1.0;
    if target <= 1.0 {
        return;
    }
    let factor = target / fr.1;
    let Some(dir) = sfnt_directory(fallback) else {
        return;
    };
    if os2_uses_typographic_metrics(fallback, dir) {
        if let Some((off, len)) = find_table(fallback, dir, b"OS/2")
            && len >= 72
        {
            scale_i16(fallback, off + 68, factor); // sTypoAscender
            scale_i16(fallback, off + 70, factor); // sTypoDescender
        }
    } else if let Some((off, len)) = find_table(fallback, dir, b"hhea")
        && len >= 8
    {
        scale_i16(fallback, off + 4, factor); // ascender
        scale_i16(fallback, off + 6, factor); // descender
    }
}

/// `(advance('m'), height())` for a font, used for the cell-width ratio.
fn advance_ratio(bytes: &[u8]) -> Option<(f32, f32)> {
    let face = rustybuzz::Face::from_slice(bytes, 0)?;
    let gid = face.glyph_index('m')?;
    let adv = face.glyph_hor_advance(gid).unwrap_or(0) as f32;
    let height = face.height() as f32;
    (adv > 0.0 && height > 0.0).then_some((adv, height))
}

/// Whether the OS/2 `USE_TYPO_METRICS` bit selects the typographic metrics
/// (ttf-parser's `ascender()`/`descender()` consult it).
fn os2_uses_typographic_metrics(bytes: &[u8], dir: usize) -> bool {
    let Some((off, len)) = find_table(bytes, dir, b"OS/2") else {
        return false;
    };
    if len < 64 || off + 64 > bytes.len() {
        return false;
    }
    u16::from_be_bytes([bytes[off + 62], bytes[off + 63]]) & 0x80 != 0
}

/// Multiply the big-endian `i16` at `at` by `factor` in place.
fn scale_i16(bytes: &mut [u8], at: usize, factor: f32) {
    if at + 2 > bytes.len() {
        return;
    }
    let v = i16::from_be_bytes([bytes[at], bytes[at + 1]]) as f32;
    let nv = (v * factor).round().clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    bytes[at..at + 2].copy_from_slice(&nv.to_be_bytes());
}

/// Offset of the first font's table directory (handles `ttcf` collections).
fn sfnt_directory(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 12 {
        return None;
    }
    if &bytes[0..4] == b"ttcf" {
        let off = read_u32(bytes, 12)? as usize;
        (off < bytes.len()).then_some(off)
    } else {
        Some(0)
    }
}

/// `(offset, length)` of a table record in the directory at `dir`.
fn find_table(bytes: &[u8], dir: usize, tag: &[u8; 4]) -> Option<(usize, usize)> {
    if dir + 12 > bytes.len() {
        return None;
    }
    let num = u16::from_be_bytes([bytes[dir + 4], bytes[dir + 5]]) as usize;
    for i in 0..num {
        let rec = dir + 12 + i * 16;
        if rec + 16 > bytes.len() {
            return None;
        }
        if &bytes[rec..rec + 4] == tag {
            return Some((read_u32(bytes, rec + 8)? as usize, read_u32(bytes, rec + 12)? as usize));
        }
    }
    None
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let b = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// One installed font file: display name (file stem) plus full path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontEntry {
    pub name: String,
    pub path: std::path::PathBuf,
    /// Monospace probe result (`None` = not probed yet, or unreadable).
    pub mono: Option<bool>,
}

impl FontEntry {
    fn new(path: std::path::PathBuf) -> Self {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        Self {
            name,
            path,
            mono: None,
        }
    }
}

/// Whether `path` looks like a font file we can load (extension check).
pub fn is_font_file(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("ttf" | "otf" | "ttc" | "otc")
    )
}

/// Probe whether a font file is really monospace (the same
/// `rustybuzz::Face::is_monospaced` check the GPU backend runs before it
/// warns). Reads and parses only — nothing is leaked, unlike [`load_set`].
/// `Err` when the file cannot be read or parsed.
pub fn is_monospaced(path: &std::path::Path) -> Result<bool, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let face = rustybuzz::Face::from_slice(&bytes, 0)
        .ok_or_else(|| format!("cannot parse {}", path.display()))?;
    Ok(face.is_monospaced())
}

/// CPU-rasterized preview: two lines (`title` big, `body` smaller), `fg`
/// text on an opaque `bg`, tightly packed RGBA8. Lets the font picker show
/// each candidate without rebuilding the GPU backend.
pub struct PreviewPixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Rasterize a two-line preview with `fontdue` (naive left-to-right layout;
/// good enough to judge a terminal face). `title_px` is the title height in
/// pixels; over-wide text shrinks to fit `MAX_W`.
pub fn render_preview(
    path: &std::path::Path,
    title: &str,
    body: &str,
    title_px: f32,
    fg: [u8; 3],
    bg: [u8; 3],
) -> Result<PreviewPixels, String> {
    const MAX_W: f32 = 1400.0;
    const MAX_H: f32 = 500.0;
    if !title_px.is_finite() || title_px <= 0.0 {
        return Err("preview size must be positive".into());
    }
    let title: String = title.chars().take(48).collect();
    if title.trim().is_empty() && body.trim().is_empty() {
        return Err("preview text is empty".into());
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;

    // Shrink once when the widest line would overflow.
    let mut px = title_px;
    for _ in 0..2 {
        let body_px = (px * 0.62).max(8.0);
        let widest = measure_line(&font, &title, px).max(measure_line(&font, body, body_px));
        if widest <= MAX_W || px <= 8.0 {
            break;
        }
        px = (px * MAX_W / widest).max(8.0);
    }
    let body_px = (px * 0.62).max(8.0);
    let pad = (px * 0.25).ceil().max(4.0);
    let gap = (px * 0.2).ceil().max(4.0);
    let (a1, h1) = line_box(&font, px);
    let (a2, h2) = line_box(&font, body_px);
    let w1 = measure_line(&font, &title, px);
    let w2 = measure_line(&font, body, body_px);
    let w = (w1.max(w2).ceil() as u32 + 2 * pad as u32).max(1);
    let h = ((h1 + gap + h2).ceil() as u32 + 2 * pad as u32).max(1).min(MAX_H as u32);
    let mut rgba = vec![0u8; (w * h) as usize * 4];
    for i in 0..(w * h) as usize {
        rgba[i * 4..i * 4 + 3].copy_from_slice(&bg);
        rgba[i * 4 + 3] = 255;
    }
    let mut y = pad;
    y = blit_line(&font, &title, px, a1, h1, pad, y, w, h, fg, &mut rgba);
    y += gap;
    blit_line(&font, body, body_px, a2, h2, pad, y, w, h, fg, &mut rgba);
    Ok(PreviewPixels {
        width: w,
        height: h,
        rgba,
    })
}

/// Advance width of one line (for measuring/fitting).
fn measure_line(font: &fontdue::Font, text: &str, px: f32) -> f32 {
    text.chars()
        .map(|c| {
            if c == '\t' {
                font.metrics(' ', px).advance_width * 4.0
            } else {
                font.metrics(c, px).advance_width
            }
        })
        .sum()
}

/// `(ascent, line height)` for `px` (fallbacks when the font lacks metrics).
fn line_box(font: &fontdue::Font, px: f32) -> (f32, f32) {
    match font.horizontal_line_metrics(px) {
        Some(m) => (m.ascent, m.ascent - m.descent),
        None => (px * 0.8, px * 1.2),
    }
}

/// Draw one line with its top at `y_top`; returns the next line top.
/// Glyph bitmaps blend `fg` over the background by coverage alpha.
#[allow(clippy::too_many_arguments)] // low-level blitter: a params struct adds no clarity
fn blit_line(
    font: &fontdue::Font,
    text: &str,
    px: f32,
    ascent: f32,
    line_h: f32,
    x0: f32,
    y_top: f32,
    w: u32,
    h: u32,
    fg: [u8; 3],
    rgba: &mut [u8],
) -> f32 {
    let baseline = y_top + ascent;
    let mut pen = x0;
    let space_adv = font.metrics(' ', px).advance_width.max(1.0);
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            continue;
        }
        if ch == '\t' {
            pen += space_adv * 4.0;
            continue;
        }
        let (m, bmp) = font.rasterize(ch, px);
        if m.width > 0 && m.height > 0 && bmp.len() == m.width * m.height {
            // Bitmap top-left in buffer coords (y grows down): xmin right of
            // the pen, ymin above the baseline minus the bitmap height.
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
            for (i, cov) in bmp.iter().enumerate() {
                if *cov == 0 {
                    continue;
                }
                let dx = gx + (i % m.width) as i32;
                let dy = gy + (i / m.width) as i32;
                if dx < 0 || dy < 0 || dx >= w as i32 || dy >= h as i32 {
                    continue;
                }
                let o = ((dy as u32 * w + dx as u32) * 4) as usize;
                let a = *cov as f32 / 255.0;
                rgba[o] = (fg[0] as f32 * a + rgba[o] as f32 * (1.0 - a)) as u8;
                rgba[o + 1] = (fg[1] as f32 * a + rgba[o + 1] as f32 * (1.0 - a)) as u8;
                rgba[o + 2] = (fg[2] as f32 * a + rgba[o + 2] as f32 * (1.0 - a)) as u8;
            }
        }
        pen += m.advance_width;
        if pen > w as f32 + 64.0 {
            break; // Off-canvas: stop early.
        }
    }
    y_top + line_h
}

/// Directories searched for installed fonts, depending on the OS.
pub fn system_font_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: std::path::PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    #[cfg(windows)]
    {
        if let Ok(root) = std::env::var("SYSTEMROOT") {
            push(std::path::PathBuf::from(format!("{root}\\Fonts")));
        } else {
            push(std::path::PathBuf::from("C:\\Windows\\Fonts"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            push(std::path::PathBuf::from(format!(
                "{local}\\Microsoft\\Windows\\Fonts"
            )));
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            push(std::path::PathBuf::from(format!(
                "{home}\\AppData\\Local\\Microsoft\\Windows\\Fonts"
            )));
        }
    }
    #[cfg(target_os = "macos")]
    {
        for d in [
            "/System/Library/Fonts",
            "/System/Library/Fonts/Supplemental",
            "/Library/Fonts",
        ] {
            push(std::path::PathBuf::from(d));
        }
        if let Ok(home) = std::env::var("HOME") {
            push(std::path::PathBuf::from(format!("{home}/Library/Fonts")));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        for d in [
            "/usr/share/fonts",
            "/usr/local/share/fonts",
            "/usr/share/texlive/texmf-dist/fonts",
        ] {
            push(std::path::PathBuf::from(d));
        }
        if let Ok(home) = std::env::var("HOME") {
            push(std::path::PathBuf::from(format!("{home}/.fonts")));
            push(std::path::PathBuf::from(format!("{home}/.local/share/fonts")));
        }
    }
    out
}

/// Every installed font file found under the system font directories,
/// sorted by display name, duplicates removed.
pub fn scan_fonts() -> Vec<FontEntry> {
    scan_dirs(&system_font_dirs())
}

/// Scan explicit directories (depth-limited recursion): the testable core
/// of [`scan_fonts`].
pub fn scan_dirs(dirs: &[std::path::PathBuf]) -> Vec<FontEntry> {
    const MAX_DEPTH: u32 = 4;
    const MAX_FILES: usize = 4000;
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for dir in dirs {
        walk_dir(dir, 0, MAX_DEPTH, &mut paths, MAX_FILES);
        if paths.len() >= MAX_FILES {
            break;
        }
    }
    paths.sort();
    paths.dedup();
    let mut entries: Vec<FontEntry> = paths.into_iter().map(FontEntry::new).collect();
    entries.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });
    entries
}

/// Collect font files under `dir`, recursing into subdirectories.
fn walk_dir(
    dir: &std::path::Path,
    depth: u32,
    max_depth: u32,
    out: &mut Vec<std::path::PathBuf>,
    max_files: usize,
) {
    if out.len() >= max_files {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    // Deterministic order (read_dir order is OS-dependent).
    let mut entries: Vec<_> = read.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if out.len() >= max_files {
            return;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < max_depth {
                walk_dir(&path, depth + 1, max_depth, out, max_files);
            }
        } else if kind.is_file() && is_font_file(&path) {
            out.push(path);
        }
    }
}
/// Common monospace font locations, best first.
fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: &str| out.push(std::path::PathBuf::from(p));
    #[cfg(windows)]
    {
        if let Ok(root) = std::env::var("SYSTEMROOT") {
            for f in ["CascadiaMono.ttf", "consola.ttf", "lucon.ttf", "cour.ttf"] {
                push(&format!("{root}\\Fonts\\{f}"));
            }
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            push(&format!(
                "{local}\\Microsoft\\Windows\\Fonts\\CascadiaMono.ttf"
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        push("/System/Library/Fonts/Menlo.ttc");
        push("/System/Library/Fonts/Monaco.ttf");
        push("/Library/Fonts/DejaVuSansMono.ttf");
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        push("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf");
        push("/usr/share/fonts/dejavu/DejaVuSansMono.ttf");
        push("/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf");
        push("/usr/share/fonts/TTF/DejaVuSansMono.ttf");
        push("/usr/share/fonts/noto/NotoSansMono-Regular.ttf");
        push("/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf");
        push("/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf");
    }
    out
}

/// Fallback font paths in priority order: the explicitly configured ones
/// verbatim, else the auto candidates that exist on disk. Mirrors what
/// [`load_set`] will actually try, for logging.
pub fn fallback_paths(configured: &[String]) -> Vec<std::path::PathBuf> {
    if !configured.is_empty() {
        return configured.iter().map(std::path::PathBuf::from).collect();
    }
    fallback_candidates()
        .into_iter()
        .filter(|p| p.is_file())
        .collect()
}
/// Cap on auto-loaded fallback fonts: bounds the one-time leak (~MBs per
/// file) while covering Nerd icons, symbols, emoji and braille.
const MAX_AUTO_FALLBACKS: usize = 6;

/// Collect automatic fallback bytes: every readable candidate, monospace
/// files first. Unparseable-but-readable files still load last (the backend
/// skips files it cannot parse, and they may still carry glyphs).
fn collect_auto_fallbacks(paths: &[std::path::PathBuf]) -> Vec<Vec<u8>> {
    let mut mono = Vec::new();
    let mut rest = Vec::new();
    for path in paths {
        if mono.len() + rest.len() >= MAX_AUTO_FALLBACKS {
            break;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let is_mono = rustybuzz::Face::from_slice(&bytes, 0)
            .map(|f| f.is_monospaced())
            .unwrap_or(false);
        if is_mono {
            mono.push(bytes);
        } else {
            rest.push(bytes);
        }
    }
    mono.into_iter().chain(rest).take(MAX_AUTO_FALLBACKS).collect()
}

/// Glyph-fallback fonts: Nerd Font icons / symbol coverage.
fn fallback_candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: &str| out.push(std::path::PathBuf::from(p));
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        for f in [
            "JetBrainsMonoNerdFont-Regular.ttf",
            "JetBrainsMonoNerdFontMono-Regular.ttf",
            "JetBrainsMonoNLNerdFont-Regular.ttf",
            "CascadiaCode.ttf",
        ] {
            push(&format!("{local}\\Microsoft\\Windows\\Fonts\\{f}"));
        }
    }
    if let Ok(root) = std::env::var("SYSTEMROOT") {
        push(&format!("{root}\\Fonts\\seguiemj.ttf")); // emoji
        push(&format!("{root}\\Fonts\\seguisym.ttf")); // symbols
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        push("/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf");
        push("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf");
    }
    #[cfg(target_os = "macos")]
    {
        push("/System/Library/Fonts/Apple Color Emoji.ttc");
    }
    out
}

#[cfg(test)]
#[path = "test/font_tests.rs"]
mod tests;
