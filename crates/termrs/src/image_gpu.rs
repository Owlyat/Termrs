//! GPU layers that sit around the terminal text.
//!
//! `sixel`/`kitty` are protocols a terminal uses to *receive* images from the
//! programs it hosts; termrs **is** the terminal, so it draws images itself.
//! This plugs into `ratatui-wgpu`'s `PostProcessor` hook and paints, in order:
//!
//! 1. an optional **backdrop** image filling the window (its `opacity` is the
//!    layer alpha - only the background is translucent),
//! 2. the composited terminal text, with the terminal's default background
//!    made transparent so the backdrop (and the desktop) show through,
//! 3. an optional **overlay** image (the `ctrl+i` viewer) on top.
//!
//! Colour correctness: `ratatui-wgpu` stores text in a *non-sRGB* texture, so
//! its samples are sRGB-encoded bytes; the output surface is sRGB, which
//! applies a linear->sRGB encode on write. The text shader therefore converts
//! sRGB->linear first, so the final values match exactly. Image layers use
//! sRGB textures, so they need no conversion.

use ratatui_wgpu::PostProcessor;
use ratatui_wgpu::wgpu;
use ratatui_wgpu::wgpu::util::DeviceExt;

use std::sync::Arc;
use std::sync::Mutex;

/// One braille dot quad, in normalised device coordinates. Braille cells are
/// drawn as geometry (see `ui::collect_braille`) instead of as a font glyph,
/// because font braille glyphs only cover part of their cell and leave
/// inconsistent row/column gaps. `ndc` is `[x0, y0, x1, y1]` with `y0` above
/// `y1`; `color` is linear RGBA (the surface sRGB-encodes on write).
#[derive(Clone, Copy, Default)]
pub struct Dot {
    pub ndc: [f32; 4],
    pub color: [f32; 4],
}

/// External state handed to the post processor at build time: the shared
/// braille dot list the UI fills each frame, plus the surface clear colour.
pub struct ProcessorUserData {
    pub braille: Arc<Mutex<Vec<Dot>>>,
    pub clear: [f64; 4],
    pub bg: [u8; 3],
}

/// Backdrop + overlay layers for the window renderer.
pub struct ImagePostProcessor {
    device: wgpu::Device,
    queue: Option<wgpu::Queue>,
    /// Base clear colour (linear, premultiplied when needed).
    clear: wgpu::Color,
    /// True when the surface expects premultiplied alpha.
    premultiplied: bool,
    text: Stage,
    image: Stage,
    /// Solid-quad pass for braille dots (no texture bindings).
    braille: wgpu::RenderPipeline,
    /// Shared dot list the UI fills every frame.
    braille_dots: Arc<Mutex<Vec<Dot>>>,
    /// Cached braille vertex buffer, grown when it is too small.
    braille_buffer: Option<wgpu::Buffer>,
    braille_cap: usize,
    linear: wgpu::Sampler,
    text_uniform: wgpu::Buffer,
    backdrop: Option<Layer>,
    overlay: Option<Layer>,
    /// Terminal cursor drawn as a thin quad (bar/underline) over the text.
    cursor: Option<Layer>,
    cursor_color: Option<[u8; 3]>,
    pending_backdrop: Option<Pending>,
    pending_overlay: Option<Pending>,
    pending_cursor: Option<Pending>,
    /// Backdrop alpha (0..1).
    opacity: f32,
    /// Overlay rectangle in physical pixels: `(x, y, w, h)`.
    rect: Option<(f32, f32, f32, f32)>,
    /// Cursor rectangle in physical pixels.
    cursor_rect: Option<(f32, f32, f32, f32)>,
}

/// A compiled pipeline plus its bind-group layout.
struct Stage {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

/// An uploaded texture with its rectangle uniform and bind group.
struct Layer {
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    uniform: wgpu::Buffer,
}

/// Texture bytes waiting for the queue to become available.
struct Pending {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    ndc: [f32; 4],
    alpha: f32,
}

const TEXT_WGSL: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
struct Params { bg: vec4<f32>, mode: vec4<f32> };

@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var<uniform> u: Params;

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let lo = c / 12.92;
    let hi = pow((c + vec3<f32>(0.055)) / 1.055, vec3<f32>(2.4));
    return select(hi, lo, c <= vec3<f32>(0.04045));
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var p = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    let xy = p[i];
    var o: VOut;
    o.pos = vec4<f32>(xy, 0.0, 1.0);
    o.uv = vec2<f32>((xy.x + 1.0) * 0.5, 1.0 - (xy.y + 1.0) * 0.5);
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(t, s, in.uv);      // sRGB-encoded bytes
    let lin = srgb_to_linear(c.rgb);          // -> linear for the sRGB target
    // Cells painted with the terminal's default background become
    // transparent, letting the backdrop/desktop show through.
    let d = abs(c.rgb - u.bg.rgb);
    let is_bg = all(d < vec3<f32>(0.004, 0.004, 0.004));
    let a = select(1.0, 0.0, is_bg) * c.a;
    var rgb = lin;
    if (u.mode.x > 0.5) {
        rgb = rgb * a; // premultiplied surface
    }
    return vec4<f32>(rgb, a);
}
"#;

const IMAGE_WGSL: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
struct Params { ndc: vec4<f32>, extra: vec4<f32> };

@group(0) @binding(0) var t: texture_2d<f32>;
@group(0) @binding(1) var s: sampler;
@group(0) @binding(2) var<uniform> p: Params;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VOut {
    var corners = array<vec2<f32>, 4>(
        vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(1.0, 1.0));
    let c = corners[i];
    let x = mix(p.ndc.x, p.ndc.z, c.x);
    let y = mix(p.ndc.y, p.ndc.w, c.y);
    var o: VOut;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = vec2<f32>(c.x, c.y);
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    let c = textureSample(t, s, in.uv); // sRGB texture -> already linear
    let a = c.a * p.extra.x;
    var rgb = c.rgb;
    if (p.extra.y > 0.5) {
        rgb = rgb * a; // premultiplied surface
    }
    return vec4<f32>(rgb, a);
}
"#;

const BRAILLE_WGSL: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) color: vec4<f32> };

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VOut {
    var o: VOut;
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    o.color = color;
    return o;
}

@fragment
fn fs(in: VOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// sRGB byte triplet (0..255) to linear [0,1].
fn srgb_u8_to_linear(v: u8) -> f64 {
    let c = v as f64 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl ImagePostProcessor {
    /// Set (or move) the terminal cursor quad. `rgb` is the cursor colour.
    /// Drawn through the image pipeline from a 1x1 texture, so the bar has no
    /// dependency on the font having a suitable glyph.
    pub fn set_cursor(&mut self, rect: (f32, f32, f32, f32), rgb: [u8; 3]) {
        self.cursor_rect = Some(rect);
        if self.cursor_color != Some(rgb) {
            self.cursor_color = Some(rgb);
            self.pending_cursor = Some(Pending {
                rgba: vec![rgb[0], rgb[1], rgb[2], 255],
                width: 1,
                height: 1,
                ndc: [0.0, 0.0, 1.0, 1.0],
                alpha: 1.0,
            });
        }
    }

    /// Hide the cursor.
    pub fn clear_cursor(&mut self) {
        self.cursor_rect = None;
    }

    /// Upload/refresh the background image layer (opacity applies only here).
    pub fn set_backdrop(&mut self, rgba: Vec<u8>, width: u32, height: u32, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
        if !self.config_ok(width, height, &rgba) || self.opacity <= 0.0 {
            self.backdrop = None;
            self.pending_backdrop = None;
            return;
        }
        self.pending_backdrop = Some(Pending {
            rgba,
            width,
            height,
            ndc: [-1.0, -1.0, 1.0, 1.0],
            alpha: self.opacity,
        });
    }

    /// Change only the backdrop alpha (no re-upload).
    pub fn set_backdrop_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(0.0, 1.0);
        if let Some(p) = &mut self.pending_backdrop {
            p.alpha = self.opacity;
        }
    }

    /// Whether a backdrop is currently uploaded.
    pub fn has_backdrop(&self) -> bool {
        self.backdrop.is_some() || self.pending_backdrop.is_some()
    }

    /// Queue the `ctrl+i` overlay image at `rect` (physical pixels).
    pub fn set_image(&mut self, rgba: Vec<u8>, width: u32, height: u32, rect: (f32, f32, f32, f32)) {
        if !self.config_ok(width, height, &rgba) {
            self.clear_image();
            return;
        }
        self.rect = Some(rect);
        self.pending_overlay = Some(Pending {
            rgba,
            width,
            height,
            ndc: [0.0, 0.0, 1.0, 1.0],
            alpha: 1.0,
        });
    }

    /// Reposition the overlay without re-uploading it.
    pub fn set_rect(&mut self, rect: (f32, f32, f32, f32)) {
        if self.overlay.is_some() {
            self.rect = Some(rect);
        }
    }

    /// Remove the overlay.
    pub fn clear_image(&mut self) {
        self.rect = None;
        self.overlay = None;
        self.pending_overlay = None;
    }

    /// True while any layer is visible (drives redraws).
    pub fn has_image(&self) -> bool {
        self.overlay.is_some()
            || self.pending_overlay.is_some()
            || self.has_backdrop()
            || self.cursor_rect.is_some()
    }

    fn config_ok(&self, width: u32, height: u32, rgba: &[u8]) -> bool {
        width > 0 && height > 0 && rgba.len() >= (width * height * 4) as usize
    }

    /// Upload any pending textures (needs the queue from `process`).
    fn flush_pending(&mut self, queue: &wgpu::Queue) {
        if let Some(p) = self.pending_backdrop.take() {
            self.backdrop = Some(self.upload(queue, &p, "backdrop"));
        }
        if let Some(p) = self.pending_overlay.take() {
            self.overlay = Some(self.upload(queue, &p, "overlay"));
        }
        if let Some(p) = self.pending_cursor.take() {
            self.cursor = Some(self.upload(queue, &p, "cursor"));
        }
    }

    fn upload(&self, queue: &wgpu::Queue, p: &Pending, label: &str) -> Layer {
        let texture = self.device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: p.width,
                    height: p.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &p.rgba,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let premult = if self.premultiplied { 1.0f32 } else { 0.0f32 };
        let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some(label),
            contents: &params2_bytes(&p.ndc, p.alpha, premult),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout: &self.image.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: uniform.as_entire_binding(),
                },
            ],
        });
        Layer {
            _texture: texture,
            bind_group,
            uniform,
        }
    }

    fn write_image_params(&self, queue: &wgpu::Queue, layer: &Layer, ndc: [f32; 4], alpha: f32) {
        let premult = if self.premultiplied { 1.0f32 } else { 0.0f32 };
        queue.write_buffer(&layer.uniform, 0, &params2_bytes(&ndc, alpha, premult));
    }

    fn ndc_for(&self, rect: (f32, f32, f32, f32), sw: f32, sh: f32) -> [f32; 4] {
        let (x, y, w, h) = rect;
        [
            x / sw * 2.0 - 1.0,
            1.0 - y / sh * 2.0,
            (x + w) / sw * 2.0 - 1.0,
            1.0 - (y + h) / sh * 2.0,
        ]
    }

    /// Pack the current braille dot list into raw vertex bytes: six vertices
    /// per quad, each `pos: [f32;2]` + `color: [f32;4]`.
    fn braille_vertex_bytes(&self) -> Vec<u8> {
        let dots = match self.braille_dots.lock() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::with_capacity(dots.len() * 6 * BRAILLE_VERTEX_BYTES);
        for d in dots.iter() {
            let [x0, y0, x1, y1] = d.ndc;
            let verts = [
                [x0, y0],
                [x1, y0],
                [x0, y1],
                [x1, y0],
                [x1, y1],
                [x0, y1],
            ];
            for pos in verts {
                for f in pos {
                    out.extend_from_slice(&f.to_ne_bytes());
                }
                for f in d.color {
                    out.extend_from_slice(&f.to_ne_bytes());
                }
            }
        }
        out
    }
}

/// Bytes per braille vertex (`pos` 2×f32 + `color` 4×f32).
const BRAILLE_VERTEX_BYTES: usize = 24;

/// `ndc + (alpha, premultiplied)` packed as two 16-byte aligned vec4s.
fn params2_bytes(ndc: &[f32; 4], alpha: f32, premult: f32) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, f) in ndc.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    out[16..20].copy_from_slice(&alpha.to_le_bytes());
    out[20..24].copy_from_slice(&premult.to_le_bytes());
    out
}

/// Text params: background chroma-key + premultiplied flag.
fn text_params_bytes(bg: [f32; 3], premult: f32) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, f) in bg.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
    }
    out[16..20].copy_from_slice(&premult.to_le_bytes());
    out
}

impl PostProcessor for ImagePostProcessor {
    /// Clear colour `[r, g, b, a]` in **sRGB** 0..1 (converted to linear here).
    type UserData = ProcessorUserData;

    fn compile(
        device: &wgpu::Device,
        _text_view: &wgpu::TextureView,
        surface_config: &wgpu::SurfaceConfiguration,
        user_data: Self::UserData,
    ) -> Self {
        let format = surface_config.format;
        let ProcessorUserData {
            braille: braille_dots,
            clear: bg,
            bg: bg_u8,
        } = user_data;
        let premultiplied = surface_config.alpha_mode
            == wgpu::CompositeAlphaMode::PreMultiplied;
        log::info!(
            "surface format={:?} alpha_mode={:?} premultiplied={premultiplied}",
            surface_config.format,
            surface_config.alpha_mode
        );
        let _transparent = surface_config.alpha_mode != wgpu::CompositeAlphaMode::Opaque;
        let linear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("termrs sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let text_uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("termrs text bg"),
            contents: &text_params_bytes(
                [
                    bg_u8[0] as f32 / 255.0,
                    bg_u8[1] as f32 / 255.0,
                    bg_u8[2] as f32 / 255.0,
                ],
                if premultiplied { 1.0 } else { 0.0 },
            ),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        // Clear: transparent surfaces clear to nothing; opaque ones to the
        // theme background. Per-pixel transparency needs a compositing alpha
        // mode, which wgpu-hal does not offer for plain HWND swapchains (its
        // caps list only `Opaque`), so on Windows the surface is always
        // opaque even when the winit window is transparent: clear opaque
        // instead of black, and revisit if the backend ever supports alpha.
        let transparent =
            surface_config.alpha_mode != wgpu::CompositeAlphaMode::Opaque && cfg!(not(windows));
        let clear = if transparent {
            wgpu::Color::TRANSPARENT
        } else {
            wgpu::Color {
                r: srgb_u8_to_linear(bg_u8[0]),
                g: srgb_u8_to_linear(bg_u8[1]),
                b: srgb_u8_to_linear(bg_u8[2]),
                a: 1.0,
            }
        };
        let _ = bg;
        Self {
            device: device.clone(),
            queue: None,
            clear,
            premultiplied,
            text: build_text_stage(device, format, premultiplied),
            image: build_image_stage(device, format, premultiplied),
            braille: build_braille_pipeline(device, format, premultiplied),
            braille_dots,
            braille_buffer: None,
            braille_cap: 0,
            linear,
            text_uniform,
            backdrop: None,
            overlay: None,
            cursor: None,
            cursor_color: None,
            pending_backdrop: None,
            pending_overlay: None,
            pending_cursor: None,
            opacity: 1.0,
            rect: None,
            cursor_rect: None,
        }
    }

    fn resize(
        &mut self,
        _device: &wgpu::Device,
        _text_view: &wgpu::TextureView,
        _surface_config: &wgpu::SurfaceConfiguration,
    ) {
        // Pipelines are format-stable; the backdrop always spans the surface.
    }

    fn process(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        queue: &wgpu::Queue,
        text_view: &wgpu::TextureView,
        surface_config: &wgpu::SurfaceConfiguration,
        surface_view: &wgpu::TextureView,
    ) {
        if self.queue.is_none() {
            self.queue = Some(queue.clone());
        }
        self.flush_pending(queue);

        let sw = surface_config.width.max(1) as f32;
        let sh = surface_config.height.max(1) as f32;

        if let Some(b) = &self.backdrop {
            self.write_image_params(queue, b, [-1.0, -1.0, 1.0, 1.0], self.opacity);
        }
        if let (Some(o), Some(rect)) = (&self.overlay, self.rect) {
            self.write_image_params(queue, o, self.ndc_for(rect, sw, sh), 1.0);
        }
        if let (Some(_), Some(rect)) = (&self.cursor, self.cursor_rect)
            && let Some(c) = &self.cursor {
                self.write_image_params(queue, c, self.ndc_for(rect, sw, sh), 1.0);
            }

        // Braille dots are solid quads drawn over the text (the UI blanks the
        // font glyphs). Uploaded through a cached, growable vertex buffer.
        let braille_bytes = self.braille_vertex_bytes();
        let braille_count = (braille_bytes.len() / BRAILLE_VERTEX_BYTES) as u32;
        if braille_count > 0 {
            if self.braille_buffer.is_none() || self.braille_cap < braille_bytes.len() {
                let cap = braille_bytes.len().next_power_of_two().max(1024);
                self.braille_buffer = Some(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("termrs braille"),
                    size: cap as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
                self.braille_cap = cap;
            }
            if let Some(buf) = &self.braille_buffer {
                queue.write_buffer(buf, 0, &braille_bytes);
            }
        }

        let text_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("termrs text bg"),
            layout: &self.text.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(text_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.linear),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: self.text_uniform.as_entire_binding(),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("termrs surface"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.clear),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });

        // 1. Backdrop (under the text; only the background is translucent).
        if let Some(b) = &self.backdrop {
            pass.set_pipeline(&self.image.pipeline);
            pass.set_bind_group(0, &b.bind_group, &[]);
            pass.draw(0..4, 0..1);
        }
        // 2. Terminal text (default background keyed out).
        pass.set_pipeline(&self.text.pipeline);
        pass.set_bind_group(0, &text_bg, &[]);
        pass.draw(0..3, 0..1);
        // 3. Braille dots (geometric; their font glyphs were blanked by the ui).
        if braille_count > 0
            && let Some(buf) = &self.braille_buffer {
                pass.set_pipeline(&self.braille);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..braille_count, 0..1);
            }
        // 4. Overlay image (ctrl+i viewer) on top.
        if let (Some(o), Some(rect)) = (&self.overlay, self.rect)
            && rect.2 > 1.0 && rect.3 > 1.0 {
                pass.set_pipeline(&self.image.pipeline);
                pass.set_bind_group(0, &o.bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
        // 5. Terminal cursor (bar/underline) above everything.
        if let (Some(c), Some(rect)) = (&self.cursor, self.cursor_rect)
            && rect.2 >= 1.0 && rect.3 >= 1.0 {
                pass.set_pipeline(&self.image.pipeline);
                pass.set_bind_group(0, &c.bind_group, &[]);
                pass.draw(0..4, 0..1);
            }
    }

    fn needs_update(&self) -> bool {
        self.has_image()
    }
}

/// Standard straight-alpha over, or premultiplied over, per surface mode.
fn blend(premultiplied: bool) -> wgpu::BlendState {
    if premultiplied {
        wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        }
    } else {
        wgpu::BlendState::ALPHA_BLENDING
    }
}

fn build_text_stage(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    premultiplied: bool,
) -> Stage {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("termrs text blit"),
        source: wgpu::ShaderSource::Wgsl(TEXT_WGSL.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("termrs text bgl"),
        entries: &[
            texture_entry(0),
            sampler_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("termrs text pl"),
        bind_group_layouts: &[&layout],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("termrs text pipeline"),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend(premultiplied)),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    Stage { pipeline, layout }
}

fn build_image_stage(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    premultiplied: bool,
) -> Stage {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("termrs image"),
        source: wgpu::ShaderSource::Wgsl(IMAGE_WGSL.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("termrs image bgl"),
        entries: &[
            texture_entry(0),
            sampler_entry(1),
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("termrs image pl"),
        bind_group_layouts: &[&layout],
        immediate_size: 0,
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("termrs image pipeline"),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend(premultiplied)),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    Stage { pipeline, layout }
}

/// Solid-quad pipeline for braille dots (vertex buffer, no bindings).
fn build_braille_pipeline(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    premultiplied: bool,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("termrs braille"),
        source: wgpu::ShaderSource::Wgsl(BRAILLE_WGSL.into()),
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("termrs braille pl"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("termrs braille pipeline"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[wgpu::VertexBufferLayout {
                array_stride: BRAILLE_VERTEX_BYTES as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend(premultiplied)),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}
