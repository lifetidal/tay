//! 渲染管线:wgpu + glyphon + cosmic-text。
//!
//! 输入是设备无关的 [`Grid`](Grid 为终端网格快照,tayd 从 alacritty_terminal
//! 提取),输出一个 wgpu 渲染帧。本 crate 不依赖 alacritty_terminal。


use std::borrow::Cow;

pub use wgpu;

use glyphon::cosmic_text::{
    Attrs, AttrsOwned, Buffer, Color, Family, FontSystem, Metrics, Shaping, Style, SwashCache,
    UnderlineStyle, Weight,
};
use glyphon::{
    Cache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use tay_config::{Config, Rgb};
use wgpu::util::DeviceExt;

/// 一个终端单元格的渲染快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// 宽字符的占位格(本身不绘制文本)。
    pub wide_spacer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
    Hidden,
}

/// 选区(单元格坐标,col,row,含两端)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: (usize, usize),
    pub end: (usize, usize),
}

/// IME 预编辑文本(显示在光标处,下划线)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    pub text: String,
    pub col: usize,
    pub row: usize,
    pub cursor_bytes: Option<(usize, usize)>,
}

/// 行尾徽章(OSC 133 退出码等)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Badge {
    pub row: usize,
    pub text: String,
    pub color: Rgb,
}

/// 单元格覆盖层(QuickSelect 标签:替换字符 + 底色块)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlay {
    pub row: usize,
    pub col: usize,
    pub text: String,
}

/// kitty graphics 图像(显示坐标)。
#[derive(Clone, Debug)]
pub struct RenderImage {
    pub id: u32,
    pub row: usize,
    pub col: usize,
    /// 占用的单元格。
    pub cols: u32,
    pub rows: u32,
    /// 像素尺寸。
    pub w: u32,
    pub h: u32,
    /// z:负 = 文本之下,正 = 文本之上。
    pub z: i32,
    pub rgba: Vec<u8>,
}

/// 终端网格快照。
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// 行优先,长度 = cols * rows。
    pub cells: Vec<Cell>,
    pub cursor: (usize, usize),
    pub cursor_shape: CursorShape,
    pub selection: Option<SelectionRange>,
    pub preedit: Option<Preedit>,
    /// 行尾徽章(行号 = 显示行)。
    pub badges: Vec<Badge>,
    /// 单元格覆盖层(QuickSelect 标签)。
    pub overlays: Vec<Overlay>,
    /// kitty graphics 图像。
    pub images: Vec<RenderImage>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Quad {
    /// x, y, w, h(物理像素)
    rect: [f32; 4],
    color: [f32; 4],
}

/// bg 四边形的管线常量。
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Locals {
    resolution: [f32; 2],
}

/// 终端渲染器:持有 wgpu 设备与 glyphon 状态。
pub struct Renderer {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub format: wgpu::TextureFormat,
    surface: wgpu::Surface<'static>,

    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    text: TextRenderer,
    viewport: Viewport,
    /// 一行一个终端行的文本缓冲。
    buffer: Buffer,

    family: String,
    /// 单元格尺寸(物理像素)。
    pub cell: (f32, f32),
    padding: f32,
    /// 当前表面尺寸(物理像素)。
    size: (u32, u32),

    fg_default: Color,
    bg_default: Rgb,
    /// surface 为 sRGB 格式时,自绘颜色需先转 linear(硬件写入时自动编码)。
    srgb: bool,

    // —— 自绘 bg 管线 ——
    bg_pipeline: wgpu::RenderPipeline,
    // —— 图像管线(kitty graphics) ——
    img_pipeline: wgpu::RenderPipeline,
    uv_quad: wgpu::Buffer,
    img_sampler: wgpu::Sampler,
    /// 内容 hash → (纹理, bind group, w, h, 实例缓冲)。
    /// 每图独立实例缓冲:共用一个会在单帧多图时发生写-读竞态。
    img_cache: std::collections::HashMap<u64, (wgpu::Texture, wgpu::BindGroup, u32, u32, wgpu::Buffer)>,
    /// 当前帧图像实例(hash, rect, z)。
    current_images: Vec<(u64, [f32; 4], i32)>,
    unit_quad: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    locals_buf: wgpu::Buffer,
    locals_bind: wgpu::BindGroup,
    quads: Vec<Quad>,

    /// TAY_SHOT:渲染 N 帧后把当前帧存为 PNG 并退出。
    shot: Option<ShotState>,
    /// 是否启用过 TAY_SHOT(与 `shot` 是否已消费区分)。
    shot_path: Option<String>,
}

struct ShotState {
    path: String,
    after_frames: u32,
    frames: u32,
}

/// FNV-1a(图像内容 hash,避免逐帧重传纹理)。
fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// sRGB 分量 → linear(surface 为 sRGB 格式时硬件会在写入时再编码回去)。
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 终端颜色 → 写入帧缓冲的颜色(sRGB surface 需转 linear)。
fn to_linear(rgb: Rgb, srgb: bool) -> [f32; 4] {
    let (r, g, b) = (rgb.0 as f32 / 255.0, rgb.1 as f32 / 255.0, rgb.2 as f32 / 255.0);
    if srgb {
        [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
    } else {
        [r, g, b, 1.0]
    }
}

fn attrs_for(family: &str, fg: Rgb, bold: bool, italic: bool, underline: bool) -> Attrs<'_> {
    Attrs::new()
        .family(Family::Name(family))
        .color(Color::rgb(fg.0, fg.1, fg.2))
        .weight(if bold { Weight::BOLD } else { Weight::NORMAL })
        .style(if italic { Style::Italic } else { Style::Normal })
        .underline(if underline { UnderlineStyle::Single } else { UnderlineStyle::None })
}

impl Renderer {
    /// 创建渲染器:`surface` 由调用方从窗口原始句柄建立(见 tayd)。
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        config: &Config,
        size: (u32, u32),
        scale_factor: f64,
        shot_path: Option<(&str, u32)>,
    ) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| format!("找不到可用的 GPU 适配器: {e}"))?;

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("tay device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .map_err(|e| format!("请求 GPU 设备失败: {e}"))?;

        let caps = surface.get_capabilities(&adapter);
        // 优先选 sRGB 格式(与 glyphon 的默认一致),否则取第一个。
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or("surface 无可用格式")?;

        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                format,
                width: size.0.max(1),
                height: size.1.max(1),
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                color_space: wgpu::SurfaceColorSpace::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );

        // —— 字体与单元格测量 ——
        let mut font_system = FontSystem::new();
        let font_size_px = (config.font.size * 96.0 / 72.0 * scale_factor as f32) as f32;
        let metrics = Metrics::new(font_size_px, font_size_px * 1.25);
        let mut probe = Buffer::new(&mut font_system, metrics);
        probe.set_size(None, None);
        let probe_attrs = attrs_for(&config.font.family, config.colors.foreground, false, false, false);
        probe.set_text("M", &probe_attrs, Shaping::Advanced, None);
        probe.shape_until_scroll(&mut font_system, false);
        let mut cell_w = font_size_px * 0.6; // 兜底:等宽近似
        let mut cell_h = metrics.line_height;
        if let Some(run) = probe.layout_runs().next() {
            cell_w = run.glyphs.iter().map(|g| g.w).sum::<f32>().max(1.0);
            cell_h = run.line_height.max(1.0);
        }
        log::info!(
            "字体 {} @ {font_size_px:.1}px → 单元格 {cell_w:.2}×{cell_h:.2}px,surface 格式 {format:?}",
            config.font.family
        );

        let mut buffer = Buffer::new(&mut font_system, Metrics::new(font_size_px, cell_h));
        let swash_cache = SwashCache::new();
        buffer.set_size(
            Some(size.0 as f32),
            Some(size.1 as f32),
        );

        // —— glyphon ——
        let cache = Cache::new(&device);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let viewport = Viewport::new(&device, &cache);

        // —— 自绘 bg 管线 ——
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tay quads"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(include_str!("quad.wgsl"))),
        });
        let locals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tay locals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bg_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tay quads layout"),
            bind_group_layouts: &[Some(&locals_layout)],
            immediate_size: 0,
        });
        let bg_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tay quads pipeline"),
            layout: Some(&bg_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[
                    // 单位 quad 角点
                    Some(wgpu::VertexBufferLayout {
                        array_stride: 8,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    }),
                    // 实例:rect + color
                    Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Quad>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![2 => Float32x4, 3 => Float32x4],
                    }),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // —— 图像管线 ——
        let img_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("tay image"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let img_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("tay image layout"),
            bind_group_layouts: &[Some(&img_layout)],
            immediate_size: 0,
        });
        let img_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("tay image"),
            source: wgpu::ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!("image.wgsl"))),
        });
        let img_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("tay image pipeline"),
            layout: Some(&img_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &img_shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[
                    Some(wgpu::VertexBufferLayout {
                        array_stride: 16,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                    }),
                    Some(wgpu::VertexBufferLayout {
                        array_stride: 16,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![2 => Float32x4],
                    }),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &img_shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uv_quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tay image quad"),
            // (pos, uv) × 4 顶点。
            contents: bytemuck::cast_slice(&[
                0.0f32, 0.0, 0.0, 0.0, //
                1.0, 0.0, 1.0, 0.0, //
                1.0, 1.0, 1.0, 1.0, //
                0.0, 0.0, 0.0, 0.0, //
                1.0, 1.0, 1.0, 1.0, //
                0.0, 1.0, 0.0, 1.0, //
            ]),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let img_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("tay image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let unit_quad = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("tay unit quad"),
            contents: bytemuck::cast_slice(&[
                0.0f32, 0.0, 1.0, 0.0, 1.0, 1.0, //
                0.0, 0.0, 1.0, 1.0, 0.0, 1.0, //
            ]),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tay quad instances"),
            size: (std::mem::size_of::<Quad>() * 4096) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let locals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tay locals"),
            size: std::mem::size_of::<Locals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let locals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tay locals bind"),
            layout: &locals_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: locals_buf.as_entire_binding(),
            }],
        });

        device.on_uncaptured_error(std::sync::Arc::new(|e| {
            log::error!("wgpu 校验错误: {e:?}");
        }));
        let mut renderer = Self {
            device,
            queue,
            format,
            surface,
            font_system,
            swash_cache,
            atlas,
            text,
            viewport,
            buffer,
            family: config.font.family.clone(),
            cell: (cell_w, cell_h),
            padding: config.padding,
            size,
            fg_default: Color::rgb(
                config.colors.foreground.0,
                config.colors.foreground.1,
                config.colors.foreground.2,
            ),
            bg_default: config.colors.background,
            srgb: format.is_srgb(),
            bg_pipeline,
            img_pipeline,
            uv_quad,
            img_sampler,
            img_cache: Default::default(),
            current_images: Vec::new(),
            unit_quad,
            instance_buf,
            locals_buf,
            locals_bind,
            quads: Vec::new(),
            shot: shot_path.map(|(p, n)| ShotState {
                path: p.into(),
                after_frames: n,
                frames: 0,
            }),
            shot_path: shot_path.map(|(p, _)| p.to_string()),
        };
        renderer.apply_locals();
        Ok(renderer)
    }

    pub fn cell_size(&self) -> (f32, f32) {
        self.cell
    }

    /// 热重载颜色(字体变化需重启:重建 FontSystem 代价高,且 cell 尺寸会变)。
    pub fn update_colors(&mut self, config: &Config) {
        self.fg_default = Color::rgb(
            config.colors.foreground.0,
            config.colors.foreground.1,
            config.colors.foreground.2,
        );
        self.bg_default = config.colors.background;
    }

    pub fn surface_size(&self) -> (u32, u32) {
        self.size
    }

    /// 取当前 surface 纹理(渲染后由调用方 present)。
    pub fn acquire_texture(&self) -> wgpu::CurrentSurfaceTexture {
        self.surface.get_current_texture()
    }

    /// TAY_SHOT 是否已抓取完成(调用方据此退出)。
    pub fn shot_taken(&self) -> bool {
        self.shot.is_none() && self.shot_path.is_some()
    }

    /// TAY_SHOT 是否仍在等待帧数(调用方据此连续重绘)。
    pub fn shot_pending(&self) -> bool {
        self.shot.is_some()
    }

    /// 表面尺寸变化(物理像素)。尺寸未变时不动 swapchain
    /// (wgpu configure 会销毁重建 swapchain,重复调用在 Mesa WSI 上会段错误)。
    pub fn resize(&mut self, width: u32, height: u32) {
        let (w, h) = (width.max(1), height.max(1));
        if self.size == (w, h) {
            return;
        }
        self.size = (w, h);
        self.surface.configure(
            &self.device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                format: self.format,
                width: w,
                height: h,
                present_mode: wgpu::PresentMode::Fifo,
                alpha_mode: wgpu::CompositeAlphaMode::Opaque,
                color_space: wgpu::SurfaceColorSpace::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );
        self.buffer.set_size(Some(w as f32), Some(h as f32));
        self.apply_locals();
    }

    fn apply_locals(&mut self) {
        let locals = Locals { resolution: [self.size.0 as f32, self.size.1 as f32] };
        self.queue.write_buffer(&self.locals_buf, 0, bytemuck::cast_slice(&[locals]));
        self.viewport.update(
            &self.queue,
            glyphon::Resolution { width: self.size.0, height: self.size.1 },
        );
    }

    /// 用新的网格快照重建文本缓冲与四边形。渲染帧之前调用。
    pub fn update(&mut self, grid: &Grid) {
        self.rebuild_text(grid);
        self.rebuild_quads(grid);
        self.rebuild_images(grid);
    }

    /// 上传新图像纹理(内容 hash 缓存)。
    fn rebuild_images(&mut self, grid: &Grid) {
        self.current_images = grid
            .images
            .iter()
            .map(|img| {
                let hash = fnv1a(&img.rgba);
                let rect = [
                    img.col as f32 * self.cell.0 + self.padding,
                    img.row as f32 * self.cell.1 + self.padding,
                    img.cols as f32 * self.cell.0,
                    img.rows as f32 * self.cell.1,
                ];
                (hash, rect, img.z)
            })
            .collect();
        for img in &grid.images {
            let hash = fnv1a(&img.rgba);
            if self.img_cache.contains_key(&hash) {
                continue;
            }
            if self.img_cache.len() > 64 {
                // 逐出最老的(hash map 无顺序,直接清一半)。
                let keys: Vec<u64> = self.img_cache.keys().copied().take(32).collect();
                for k in keys {
                    self.img_cache.remove(&k);
                }
            }
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tay kitty image"),
                size: wgpu::Extent3d {
                    width: img.w,
                    height: img.h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            self.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &img.rgba,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(img.w * 4),
                    rows_per_image: Some(img.h),
                },
                wgpu::Extent3d {
                    width: img.w,
                    height: img.h,
                    depth_or_array_layers: 1,
                },
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tay kitty image bind"),
                layout: &self
                    .img_pipeline
                    .get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.locals_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.img_sampler),
                    },
                ],
            });
            let inst = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tay image inst"),
                size: 16,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.img_cache.insert(hash, (texture, bind, img.w, img.h, inst));
        }
    }

    /// 网格 → cosmic-text:每行按样式分段,(row 之间用 `\n` span 分行)。
    fn rebuild_text(&mut self, grid: &Grid) {
        let family = self.family.clone();
        let default_attrs = attrs_for(&family, grid_default_fg(grid), false, false, false);

        // 光标格(块状)文本取反色。
        let cursor_cell = grid.cursor_shape == CursorShape::Block;
        let cursor_pos = grid.cursor;

        let mut spans: Vec<(String, AttrsOwned)> = Vec::new();
        let mut cur: Option<(String, AttrsOwned)> = None;
        let push_run = |spans: &mut Vec<(String, AttrsOwned)>, cur: &mut Option<(String, AttrsOwned)>| {
            if let Some(t) = cur.take() {
                spans.push(t);
            }
        };

        for row in 0..grid.rows {
            // 行字符缓冲(宽字符占位格跳过),overlay 直接替换字符。
            let mut row_chars: Vec<char> = grid.cells[row * grid.cols..(row + 1) * grid.cols]
                .iter()
                .filter(|c| !c.wide_spacer)
                .map(|c| c.c)
                .collect();
            let row_overlays: Vec<&Overlay> =
                grid.overlays.iter().filter(|o| o.row == row).collect();
            if !row_overlays.is_empty() {
                let base_cols = row_chars.len();
                for o in &row_overlays {
                    for (i, ch) in o.text.chars().enumerate() {
                        let pos = o.col + i;
                        if pos < base_cols {
                            row_chars[pos] = ch;
                        }
                    }
                }
            }
            let mut col = 0; // 当前字符序号(同时是列号,宽字符占 2 列在 M3 不精确处理)
            for cell in &grid.cells[row * grid.cols..(row + 1) * grid.cols] {
                if cell.wide_spacer {
                    continue;
                }
                // 预编辑文本插在光标格之后(光标保持可见)。
                if grid.preedit.as_ref().is_some_and(|p| p.row == row && p.col == col) {
                    push_run(&mut spans, &mut cur);
                    let pre = grid.preedit.as_ref().unwrap();
                    let attrs = Attrs::new()
                        .family(Family::Name(&family))
                        .color(Color::rgb(0xff, 0xb4, 0x5c))
                        .underline(UnderlineStyle::Single);
                    spans.push((pre.text.clone(), AttrsOwned::new(&attrs)));
                }
                let cell = cell;
                if cell.wide_spacer {
                    continue;
                }
                // 块状光标格:文本用底色、底色用光标色。
                let fg = if cursor_cell && (col, row) == cursor_pos {
                    cell.bg
                } else {
                    cell.fg
                };
                let attrs = attrs_for(&family, fg, cell.bold, cell.italic, cell.underline);
                let ch = row_chars.get(col).copied().unwrap_or(cell.c);
                match &mut cur {
                    Some((text, a)) if *a == AttrsOwned::new(&attrs) => text.push(ch),
                    _ => {
                        push_run(&mut spans, &mut cur);
                        cur = Some((ch.to_string(), AttrsOwned::new(&attrs)));
                    },
                }
                col += 1;
            }
            push_run(&mut spans, &mut cur);

            // 行尾徽章:替换行末字符(通常是提示符行尾的空格)。
            if let Some(badge) = grid.badges.iter().find(|b| b.row == row) {
                let need = badge.text.chars().count();
                let mut to_drop = need;
                while to_drop > 0 {
                    let Some((text, _)) = spans.last_mut() else { break };
                    let d = text.chars().count().min(to_drop);
                    let keep = text.chars().count() - d;
                    *text = text.chars().take(keep).collect();
                    to_drop -= d;
                    if text.is_empty() {
                        spans.pop();
                    }
                }
                if to_drop == 0 {
                    let attrs = Attrs::new()
                        .family(Family::Name(&family))
                        .color(Color::rgb(badge.color.0, badge.color.1, badge.color.2))
                        .weight(Weight::BOLD);
                    spans.push((badge.text.clone(), AttrsOwned::new(&attrs)));
                }
            }

            if row + 1 < grid.rows {
                spans.push(("\n".into(), AttrsOwned::new(&default_attrs)));
            }
        }

        let default_attrs_borrowed = default_attrs;
        self.buffer.set_rich_text(
            spans.iter().map(|(s, a)| (s.as_str(), a.as_attrs())),
            &default_attrs_borrowed,
            Shaping::Advanced,
            None,
        );
        self.buffer.shape_until_scroll(&mut self.font_system, false);
    }

    /// 单元格底色 + 选区 + 光标 → 四边形实例。
    fn rebuild_quads(&mut self, grid: &Grid) {
        self.quads.clear();
        let (cw, ch) = self.cell;
        let pad = self.padding;

        // 每行合并相同底色。
        for row in 0..grid.rows {
            let base = row * grid.cols;
            let mut col = 0;
            while col < grid.cols {
                let bg = grid.cells[base + col].bg;
                let start = col;
                while col + 1 < grid.cols && grid.cells[base + col + 1].bg == bg {
                    col += 1;
                }
                self.quads.push(Quad {
                    rect: [
                        start as f32 * cw + pad,
                        row as f32 * ch + pad,
                        (col - start + 1) as f32 * cw,
                        ch,
                    ],
                    color: to_linear(bg, self.srgb),
                });
                col += 1;
            }
        }

        // 选区。
        if let Some(sel) = grid.selection {
            let (sx, sy) = sel.start;
            let (ex, ey) = sel.end;
            for row in sy..=ey {
                let (x0, x1) = if row == sy && row == ey {
                    (sx, ex + 1)
                } else if row == sy {
                    (sx, grid.cols)
                } else if row == ey {
                    (0, ex + 1)
                } else {
                    (0, grid.cols)
                };
                self.quads.push(Quad {
                    rect: [x0 as f32 * cw + pad, row as f32 * ch + pad, (x1 - x0) as f32 * cw, ch],
                    color: to_linear(Rgb::new(0x39, 0x5e, 0x9c), self.srgb),
                });
            }
        }

        // QuickSelect overlay 底色块(覆盖单元格底色)。
        for o in &grid.overlays {
            self.quads.push(Quad {
                rect: [
                    o.col as f32 * cw + pad,
                    o.row as f32 * ch + pad,
                    o.text.chars().count() as f32 * cw,
                    ch,
                ],
                color: to_linear(Rgb::new(0x00, 0x8b, 0x74), self.srgb),
            });
        }

        // 光标。
        if grid.cursor_shape != CursorShape::Hidden {
            let (cx, cy) = grid.cursor;
            let (x, y, w, h) = match grid.cursor_shape {
                CursorShape::Block => (cx as f32 * cw + pad, cy as f32 * ch + pad, cw, ch),
                CursorShape::Underline => (
                    cx as f32 * cw + pad,
                    (cy + 1) as f32 * ch + pad - ch * 0.12,
                    cw,
                    ch * 0.12,
                ),
                CursorShape::Beam => (cx as f32 * cw + pad, cy as f32 * ch + pad, cw * 0.1, ch),
                CursorShape::Hidden => unreachable!(),
            };
            self.quads.push(Quad {
                rect: [x, y, w, h],
                color: to_linear(Rgb::new(0x00, 0xa8, 0x84), self.srgb),
            });
        }

        // 上传实例(超出容量则扩容)。
        let needed = (self.quads.len().max(1) * std::mem::size_of::<Quad>()) as u64;
        if needed > self.instance_buf.size() {
            self.instance_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tay quad instances"),
                size: needed.next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(&self.quads));
    }

    /// 渲染一帧到 `texture`(当前 surface 纹理)并 present。
    pub fn render(&mut self, texture: wgpu::SurfaceTexture, alpha: f32) -> Result<(), String> {
        let view = texture.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("tay frame") });

        let text_area = TextArea {
            buffer: &self.buffer,
            left: self.padding,
            top: self.padding,
            scale: 1.0,
            bounds: TextBounds::default(),
            default_color: self.fg_default,
            custom_glyphs: &[],
        };
        self.text
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                [text_area],
                &mut self.swash_cache,
            )
            .map_err(|e| format!("文本准备失败: {e}"))?;

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tay pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: to_linear(self.bg_default, self.srgb)[0] as f64,
                            g: to_linear(self.bg_default, self.srgb)[1] as f64,
                            b: to_linear(self.bg_default, self.srgb)[2] as f64,
                            a: alpha as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if !self.quads.is_empty() {
                pass.set_pipeline(&self.bg_pipeline);
                pass.set_bind_group(0, &self.locals_bind, &[]);
                pass.set_vertex_buffer(0, self.unit_quad.slice(..));
                pass.set_vertex_buffer(1, self.instance_buf.slice(..));
                pass.draw(0..6, 0..self.quads.len() as u32);
            }
            // 图像:z<=0 在文本之下。
            self.draw_images(&mut pass, false);
            self.text
                .render(&self.atlas, &self.viewport, &mut pass)
                .map_err(|e| format!("文本渲染失败: {e}"))?;
            // 图像:z>0 在文本之上。
            self.draw_images(&mut pass, true);
        }

        // TAY_SHOT:渲染 N 帧后抓取当前帧并退出(capture 消耗 encoder)。
        if let Some(shot) = &mut self.shot {
            shot.frames += 1;
            if shot.frames >= shot.after_frames {
                let path = shot.path.clone();
                self.shot = None;
                self.capture(encoder, &texture.texture, &path);
                return Ok(());
            }
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(texture);
        Ok(())
    }

    /// 画 kitty 图像(`above` = 文本之上)。
    fn draw_images(&mut self, pass: &mut wgpu::RenderPass<'_>, above: bool) {
        let rects: Vec<(u64, [f32; 4])> = self
            .current_images
            .iter()
            .filter(|(_, _, z)| (*z > 0) == above)
            .map(|(hash, rect, _)| (*hash, *rect))
            .collect();
        for (hash, rect) in rects {
            let Some((_, bind, _, _, inst)) = self.img_cache.get(&hash) else { continue };
            self.queue.write_buffer(inst, 0, bytemuck::cast_slice(&[rect]));
            pass.set_pipeline(&self.img_pipeline);
            pass.set_bind_group(0, bind, &[]);
            pass.set_vertex_buffer(0, self.uv_quad.slice(..));
            pass.set_vertex_buffer(1, inst.slice(..));
            pass.draw(0..6, 0..1);
        }
    }

    /// 把当前帧纹理读回并保存为 PNG(阻塞)。
    fn capture(&mut self, mut encoder: wgpu::CommandEncoder, texture: &wgpu::Texture, path: &str) {
        let size = texture.size();
        let (w, h) = (size.width, size.height);
        // BGRA 每行 256 字节对齐。
        let bpr = (w * 4).next_multiple_of(256);
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("tay shot"),
            size: bpr as u64 * h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::PollType::wait_indefinitely()).ok();
        if rx.recv().is_err() {
            log::error!("截图读回失败");
            return;
        }
        let data = slice.get_mapped_range().map_err(|e| log::error!("映射截图缓冲失败: {e}")).ok();
        let Some(data) = data else { return };
        // 去掉行填充;Bgra 格式交换 R/B。
        let swap = matches!(
            self.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            let src = y * bpr as usize;
            let dst = y * (w * 4) as usize;
            for x in 0..w as usize {
                rgba[dst + x * 4] = data[src + x * 4 + if swap { 2 } else { 0 }];
                rgba[dst + x * 4 + 1] = data[src + x * 4 + 1];
                rgba[dst + x * 4 + 2] = data[src + x * 4 + if swap { 0 } else { 2 }];
                rgba[dst + x * 4 + 3] = 255;
            }
        }
        drop(data);
        buf.unmap();
        if let Err(e) = image::save_buffer(path, &rgba, w, h, image::ExtendedColorType::Rgba8) {
            log::error!("截图保存失败: {e}");
        } else {
            log::info!("截图已保存: {path}");
        }
    }
}

/// 网格首格的默认前景色(空行 span 用)。
fn grid_default_fg(grid: &Grid) -> Rgb {
    grid.cells.first().map(|c| c.fg).unwrap_or(Rgb::new(0xd4, 0xd4, 0xd4))
}
