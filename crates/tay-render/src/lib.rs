//! 渲染管线:wgpu + glyphon + cosmic-text。
//!
//! 输入是设备无关的 [`Grid`](Grid 为终端网格快照,tayd 从 alacritty_terminal
//! 提取),输出一个 wgpu 渲染帧。本 crate 不依赖 alacritty_terminal。

mod gpu_core;
pub use crate::gpu_core::GpuCore;
mod wallpaper;
use std::borrow::Cow;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

pub use wgpu;

use glyphon::cosmic_text::{
    Attrs, Buffer, Color, Family, FeatureTag, FontFeatures, FontSystem, Metrics, Shaping, Style,
    SwashCache, UnderlineStyle, Weight,
};
use glyphon::{Cache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport};
use tay_config::{Config, Rgb};
use wgpu::util::DeviceExt;

use crate::gpu_core::{CacheDecision, FontKey, FormatCache, GlyphSet};

/// 一个终端单元格的渲染快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub c: char,
    pub zerowidth: Option<Arc<str>>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowIconKind {
    Minimize,
    Maximize,
    Restore,
    Close,
    Settings,
    Add,
}
#[derive(Clone, Copy, Debug)]
pub struct WindowIcon {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub kind: WindowIconKind,
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
    /// Exclusive grid column used to clip IME ink to its owning pane.
    pub clip_right: Option<usize>,
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
    /// Optional pane clipping rectangle in grid cells (x,y,width,height).
    pub clip: Option<(usize, usize, usize, usize)>,
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
    /// 像素共享句柄(一次解码,放置/渲染共享引用 —— 架构 §6)。
    pub rgba: std::sync::Arc<[u8]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelCorners {
    All,
    Opposite,
}

/// 栏内圆角面板：背景由网格着色，这里绘制圆角遮罩及活动标记。
#[derive(Clone, Debug)]
pub struct ChromePanel {
    pub row: usize,
    /// Explicit height for modal panels; zero uses the header row height.
    pub rows: usize,
    pub start: usize,
    pub end: usize,
    pub background: Rgb,
    pub accent: Option<Rgb>,
    pub corners: PanelCorners,
}

/// position = 历史偏移比例：0 最新，1 最旧。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrollbar {
    /// Optional physical right edge for a pane-local scrollbar.
    pub right: Option<f32>,
    pub top: f32,
    pub extent: f32,
    pub position: f32,
    pub size: f32,
}
#[derive(Clone, Copy, Debug)]
pub struct ScrollbarGeometry {
    pub x: f32,
    pub width: f32,
    pub top: f32,
    pub height: f32,
    pub thumb_top: f32,
    pub thumb_height: f32,
}
impl Scrollbar {
    pub fn geometry(self, right: f32, cell_height: f32, padding: f32) -> Option<ScrollbarGeometry> {
        if self.size >= 1.0 || self.extent <= 0.0 {
            return None;
        }
        let right = self.right.unwrap_or(right);
        let height = self.extent * cell_height;
        let thumb_height = (self.size * height)
            .max(24.0)
            .min((height - 2.0).max(height * 0.5));
        let top = padding + self.top * cell_height;
        let width = (cell_height * 0.65).clamp(12.0, 28.0);
        Some(ScrollbarGeometry {
            x: (right - width).max(0.0),
            width,
            top,
            height,
            thumb_top: top + (1.0 - self.position.clamp(0.0, 1.0)) * (height - thumb_height),
            thumb_height,
        })
    }
}
impl ScrollbarGeometry {
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.top && y < self.top + self.height
    }
    pub fn offset(self, y: f32, grab: f32, history: usize) -> usize {
        let travel = self.height - self.thumb_height;
        if travel <= 0.0 {
            return 0;
        }
        ((1.0 - ((y - self.top - grab) / travel).clamp(0.0, 1.0)) * history as f32).round() as usize
    }
}

/// 终端网格快照。
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    /// 行优先,长度 = cols * rows。
    pub cells: Vec<Cell>,
    pub cursor: (usize, usize),
    pub cursor_shape: CursorShape,
    pub window_icons: Vec<WindowIcon>,
    pub chrome_panels: Vec<ChromePanel>,
    /// 顶部标签栏占用的网格行数；文字与图标在此高度内居中。
    pub tab_bar_rows: usize,
    /// 行 0 面板/图标/字形实际铺的行数(≤ tab_bar_rows):有消息行时
    /// 比栏少 1,消息行落在面板之外,不会被标签圆角面板盖住。
    pub tab_panel_rows: usize,
    /// Modal UI must stay opaque even when a cell uses the terminal default color.
    pub opaque_background: bool,
    pub scrollbar: Option<Scrollbar>,
    pub selection: Option<SelectionRange>,
    /// 搜索命中行高亮(显示坐标;与选区色分开,见 Renderer)。
    pub search_hit: Option<SelectionRange>,
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

struct ImageDraw {
    key: (u32, usize),
    rect: [f32; 4],
    z: i32,
    wallpaper: bool,
    clip: [u32; 4],
}

impl ImageDraw {
    fn new(
        img: &RenderImage,
        wallpaper: bool,
        cell: (f32, f32),
        padding: f32,
        size: (u32, u32),
    ) -> Self {
        let key = (
            img.id,
            std::sync::Arc::as_ptr(&img.rgba) as *const () as usize,
        );
        let mut rect = [
            img.col as f32 * cell.0 + padding,
            img.row as f32 * cell.1 + padding,
            img.cols as f32 * cell.0,
            img.rows as f32 * cell.1,
        ];
        if wallpaper {
            let scale = (size.0 as f32 / img.w as f32).max(size.1 as f32 / img.h as f32);
            let w = img.w as f32 * scale;
            let h = img.h as f32 * scale;
            rect = [(size.0 as f32 - w) / 2.0, (size.1 as f32 - h) / 2.0, w, h];
        }
        let clip = img
            .clip
            .map(|(x, y, w, h)| {
                let left = (padding + x as f32 * cell.0).floor().max(0.0) as u32;
                let top = (padding + y as f32 * cell.1).floor().max(0.0) as u32;
                let right = (padding + (x + w) as f32 * cell.0).ceil().max(0.0) as u32;
                let bottom = (padding + (y + h) as f32 * cell.1).ceil().max(0.0) as u32;
                [
                    left.min(size.0),
                    top.min(size.1),
                    right.min(size.0).saturating_sub(left),
                    bottom.min(size.1).saturating_sub(top),
                ]
            })
            .unwrap_or([0, 0, size.0, size.1]);
        ImageDraw {
            key,
            rect,
            z: img.z,
            wallpaper,
            clip,
        }
    }
    fn in_pass(&self, above: bool, wallpaper: bool) -> bool {
        if wallpaper {
            self.wallpaper
        } else {
            !self.wallpaper && (self.z > 0) == above
        }
    }
}

/// bg 四边形的管线常量。
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Locals {
    resolution: [f32; 2],
}

/// 终端渲染器:持有 wgpu 设备与 glyphon 状态。
pub struct Renderer {
    pub format: wgpu::TextureFormat,
    alpha_mode: wgpu::CompositeAlphaMode,
    surface: wgpu::Surface<'static>,
    /// device 级资源:由 TayApp 在 SurfaceReplaced 时抽出保留,
    /// bind 时重新挂回。None 仅在 take_core 与 bind 之间短暂出现。
    core: Option<GpuCore>,

    glyph_draws: Vec<GlyphDraw>,
    tab_bar_rows: usize,
    tab_panel_rows: usize,
    preedit_pos: Option<(usize, usize)>,
    preedit_right: Option<usize>,

    /// 单元格尺寸(物理像素)。
    pub cell: (f32, f32),
    padding: f32,
    /// 当前表面尺寸(物理像素)。
    size: (u32, u32),

    fg_default: Color,
    bg_default: Rgb,
    /// 光标/选区色(config colors.cursor / colors.selection,
    /// 热重载即时生效 —— 此前硬编码,用户在 Lua 里改这两个
    /// 颜色不生效,2026-10-19 修复)。
    cursor_color: Rgb,
    selection_color: Rgb,
    search_hit_color: Rgb,

    /// 当前帧图像实例((id, ptr), rect, z)。
    current_images: Vec<ImageDraw>,
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
/// 图像纹理缓存条目。
pub(crate) type ImageCache = std::collections::HashMap<
    (u32, usize),
    (wgpu::Texture, wgpu::BindGroup, u32, u32, wgpu::Buffer),
>;

/// sRGB 分量 → linear(surface 为 sRGB 格式时硬件会在写入时再编码回去)。
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 终端颜色 → 写入帧缓冲的颜色(sRGB surface 需转 linear)。
/// 通道线性插值(0..1,钳制);搜索命中色等派生色用。
fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let lerp = |x: u8, y: u8| {
        ((x as f32 + (y as f32 - x as f32) * t)
            .round()
            .clamp(0.0, 255.0)) as u8
    };
    Rgb::new(lerp(a.0, b.0), lerp(a.1, b.1), lerp(a.2, b.2))
}

fn to_linear(rgb: Rgb, srgb: bool) -> [f32; 4] {
    let (r, g, b) = (
        rgb.0 as f32 / 255.0,
        rgb.1 as f32 / 255.0,
        rgb.2 as f32 / 255.0,
    );
    if srgb {
        [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b), 1.0]
    } else {
        [r, g, b, 1.0]
    }
}

#[derive(Clone, Debug)]
struct GlyphDraw {
    cell: Cell,
    col: usize,
    row: usize,
    width: usize,
}
pub(crate) type GlyphKey = (char, Option<Arc<str>>, u32, u8);

pub(crate) struct CachedGlyph {
    buffer: Buffer,
    ink: Option<GlyphInk>,
}

#[derive(Clone, Copy, Debug)]
struct GlyphInk {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl GlyphDraw {
    fn key(&self) -> GlyphKey {
        (
            self.cell.c,
            self.cell.zerowidth.clone(),
            self.cell.fg.to_u32(),
            self.cell.bold as u8 | (self.cell.italic as u8) << 1 | (self.cell.underline as u8) << 2,
        )
    }
}

fn is_icon(c: char) -> bool {
    matches!(c as u32, 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd)
}

/// Fit raster ink, not advance width; column origins never accumulate advances.
fn fit_glyph(ink: Option<GlyphInk>, c: char, columns: usize, cell: (f32, f32)) -> (f32, f32, f32) {
    let Some(ink) = ink else {
        return (0.0, 0.0, 1.0);
    };
    // Powerline separators intentionally bleed over cell edges for seamless joins.
    if matches!(c as u32, 0xe0b0..=0xe0d7) {
        return (0.0, 0.0, 1.0);
    }
    let width = ink.right - ink.left;
    let height = ink.bottom - ink.top;
    let scale = (columns as f32 * cell.0 / width.max(1.0))
        .min(cell.1 / height.max(1.0))
        .min(1.0);
    let dx = if scale < 1.0 {
        (columns as f32 * cell.0 - width * scale) * 0.5 - ink.left * scale
    } else if ink.left < 0.0 {
        -ink.left
    } else {
        (columns as f32 * cell.0 - ink.right).min(0.0)
    };
    let dy = if scale < 1.0 {
        (cell.1 - height * scale) * 0.5 - ink.top * scale
    } else if ink.top < 0.0 {
        -ink.top
    } else {
        (cell.1 - ink.bottom).min(0.0)
    };
    (dx, dy, scale)
}

fn text_cells(grid: &Grid, cursor_color: Rgb) -> Vec<GlyphDraw> {
    use unicode_width::UnicodeWidthChar;
    let mut cells = grid.cells.clone();
    let mut put = |row: usize, col: usize, text: &str, color: Option<Rgb>| {
        if row >= grid.rows {
            return;
        }
        let mut x = col;
        for ch in text.chars() {
            let width = ch.width().unwrap_or(1).max(1);
            if x + width > grid.cols {
                break;
            }
            let cell = &mut cells[row * grid.cols + x];
            cell.c = ch;
            cell.zerowidth = None;
            cell.wide_spacer = false;
            if let Some(fg) = color {
                cell.fg = fg;
                cell.bold = true;
            }
            for off in 1..width {
                cells[row * grid.cols + x + off].wide_spacer = true;
            }
            x += width;
        }
    };
    for overlay in &grid.overlays {
        put(overlay.row, overlay.col, &overlay.text, None);
    }
    for badge in &grid.badges {
        put(
            badge.row,
            grid.cols.saturating_sub(badge.text.width()),
            &badge.text,
            Some(badge.color),
        );
    }
    let mut draws = Vec::new();
    for row in 0..grid.rows {
        for col in 0..grid.cols {
            let mut cell = cells[row * grid.cols + col].clone();
            if cell.wide_spacer {
                continue;
            }
            let width = if col + 1 < grid.cols
                && (cells[row * grid.cols + col + 1].wide_spacer
                    || (is_icon(cell.c) && {
                        let next = &cells[row * grid.cols + col + 1];
                        next.c == ' '
                            && next.zerowidth.is_none()
                            && !next.underline
                            && next.bg == cell.bg
                            && grid.cursor != (col + 1, row)
                    })) {
                2
            } else {
                1
            };
            if grid.preedit.as_ref().is_some_and(|p| {
                p.row == row && col < p.col + p.text.width() && col + width > p.col
            }) {
                continue;
            }
            if grid.cursor_shape == CursorShape::Block
                && grid.preedit.is_none()
                && grid.cursor == (col, row)
            {
                cell.fg = contrast_text(cursor_color);
            }
            if cell.c == ' ' && cell.zerowidth.is_none() && !cell.underline {
                continue;
            }
            draws.push(GlyphDraw {
                cell,
                col,
                row,
                width,
            });
        }
    }
    draws
}

fn contrast_text(bg: Rgb) -> Rgb {
    if (bg.0 as u32 * 299 + bg.1 as u32 * 587 + bg.2 as u32 * 114) > 150000 {
        Rgb(18, 22, 28)
    } else {
        Rgb(245, 247, 250)
    }
}

fn attrs_for(
    family: &str,
    weight: u16,
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
) -> Attrs<'_> {
    let mut features = FontFeatures::new();
    for tag in [b"liga", b"clig", b"calt", b"kern"] {
        features.disable(FeatureTag::new(tag));
    }
    Attrs::new()
        .font_features(features)
        .family(if family.eq_ignore_ascii_case("monospace") {
            Family::Monospace
        } else {
            Family::Name(family)
        })
        .color(Color::rgb(fg.0, fg.1, fg.2))
        .weight(if bold { Weight::BOLD } else { Weight(weight) })
        .style(if italic { Style::Italic } else { Style::Normal })
        .underline(if underline {
            UnderlineStyle::Single
        } else {
            UnderlineStyle::None
        })
}

impl Renderer {
    /// bind:core 为 None 时全新构建(首次初始化);Some 时复用常驻资源,
    /// 仅字体键或 surface format 变化时重建对应部分。
    pub fn bind(
        core_in: Option<GpuCore>,
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        config: &Config,
        size: (u32, u32),
        scale_factor: f64,
        shot_path: Option<(&str, u32)>,
    ) -> Result<Self, String> {
        let (mut core, format, alpha_mode) = match core_in {
            Some(mut core) => {
                log::info!("GpuCore 复用(Instance/Device 不重建)");
                if core.ensure_device(instance, &surface)? {
                    // 混合 GPU 迁移:device 已重建,全部 device 级缓冲与缓存
                    // 必须同步重建,否则跨 device 校验 panic。
                    let (uv_quad, img_sampler, unit_quad, instance_buf, locals_buf, locals_bind) =
                        build_buffers(&core.device);
                    core.uv_quad = uv_quad;
                    core.img_sampler = img_sampler;
                    core.unit_quad = unit_quad;
                    core.instance_buf = instance_buf;
                    core.locals_buf = locals_buf;
                    core.locals_bind = locals_bind;
                    core.img_cache.clear();
                    core.wallpaper = None;
                    core.background_checked = None;
                    core.glyph_buffers.clear();
                }
                let (format, alpha_mode) = negotiate_configure(
                    &surface,
                    core.adapter.as_ref().expect("adapter 已就绪"),
                    &core.device,
                    size,
                )?;
                if core.font_key != font_key_of(config, scale_factor) {
                    let (font_system, swash_cache, metrics, size_px, _cell) =
                        build_fonts(config, scale_factor);
                    core.font_system = font_system;
                    core.swash_cache = swash_cache;
                    core.font_metrics = metrics;
                    core.font_key = FontKey {
                        family: config.font.family.clone(),
                        weight: config.font.weight,
                        size_px,
                    };
                    core.glyph_buffers.clear();
                    core.glyph_sets = FormatCache::default();
                }
                match core.glyph_sets.decide(format) {
                    CacheDecision::Reuse => {}
                    _ => {
                        core.glyph_sets.current =
                            Some((format, build_glyph_set(&core.device, &core.queue, format)?));
                    }
                }
                (core, format, alpha_mode)
            }
            None => build_fresh(instance, &surface, config, size, scale_factor)?,
        };

        // —— 表面层字段(字体 probe 复用常驻 FontSystem,仅测单元格) ——
        let (cell_w, cell_h) =
            probe_cell(&mut core.font_system, config, scale_factor, Some(format));
        let font_size_px = core.font_key.size_px;
        let mut preedit_buffer =
            Buffer::new(&mut core.font_system, Metrics::new(font_size_px, cell_h));
        preedit_buffer.set_wrap(glyphon::cosmic_text::Wrap::None);
        preedit_buffer.set_hinting(glyphon::cosmic_text::Hinting::Enabled);
        preedit_buffer.set_monospace_width(Some(cell_w));
        core.preedit_buffer = preedit_buffer;

        let mut renderer = Self {
            format,
            alpha_mode,
            surface,
            core: Some(core),
            glyph_draws: Vec::new(),
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            preedit_pos: None,
            preedit_right: None,
            cell: (cell_w, cell_h),
            padding: config.padding * scale_factor as f32,
            size,
            fg_default: Color::rgb(
                config.colors.foreground.0,
                config.colors.foreground.1,
                config.colors.foreground.2,
            ),
            bg_default: config.colors.background,
            cursor_color: config.colors.cursor,
            selection_color: config.colors.selection,
            search_hit_color: mix(config.colors.cursor, config.colors.background, 0.55),
            current_images: Vec::new(),
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

    fn core(&mut self) -> &mut GpuCore {
        self.core.as_mut().expect("GpuCore 未挂载")
    }

    /// 抽出常驻资源(SurfaceReplaced 前调用;旧 surface 资源随本 Renderer drop)。
    pub fn take_core(&mut self) -> GpuCore {
        self.core.take().expect("GpuCore 未挂载")
    }

    pub fn padding(&self) -> f32 {
        self.padding
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
        let background = (
            config.background_image.clone(),
            config.background_image_opacity,
            config.background_image_brightness,
        );
        // 滑条每步都会换 key:保留旧壁纸作陈旧回退,worker 解码新 key
        // 期间不闪屏(poll_background 的 loaded.or_else(wallpaper) 兜底);
        // img_cache 是 kitty 内联图,与壁纸无关,清掉会让它们每步重传。
        self.core.as_mut().expect("GpuCore 未挂载").wallpaper_config = background;
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .background_checked = None;
        self.poll_background();
        self.cursor_color = config.colors.cursor;
        self.selection_color = config.colors.selection;
        self.search_hit_color = mix(config.colors.cursor, config.colors.background, 0.55);
    }

    pub fn poll_background(&mut self) -> bool {
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        if core
            .background_checked
            .is_some_and(|at| at.elapsed() < std::time::Duration::from_millis(250))
        {
            return false;
        }
        core.background_checked = Some(std::time::Instant::now());
        let loaded = wallpaper::request(
            &core.wallpaper_config.0,
            core.wallpaper_config.1,
            core.wallpaper_config.2,
        );
        let next = if core.wallpaper_config.0.is_empty() {
            None
        } else {
            // A busy cache or pending retry must not flash a loaded background away.
            loaded.or_else(|| core.wallpaper.clone())
        };
        let changed = match (&core.wallpaper, &next) {
            (Some(a), Some(b)) => !Arc::ptr_eq(&a.rgba, &b.rgba),
            (None, None) => false,
            _ => true,
        };
        if changed {
            core.wallpaper = next;
            // img_cache(kitty 内联图,按图像 id 缓存)与壁纸无关:
            // 不清 —— 滑条每步换壁纸时内联图不用重传。
        }
        changed
    }
    pub fn has_background_image(&mut self) -> bool {
        !self
            .core
            .as_mut()
            .expect("GpuCore 未挂载")
            .wallpaper_config
            .0
            .is_empty()
    }

    /// 最近一次背景图片解码失败原因(设置面板显示;成功或无图时为 None)。
    pub fn background_error(&self) -> Option<String> {
        wallpaper::last_error()
    }

    /// 更新字体与字号，复用 GPU 和会话；调用方同步重算网格。
    pub fn update_font(&mut self, config: &Config, scale: f64) {
        let px = config.font.size * 96.0 / 72.0 * scale as f32;
        let metrics = Metrics::new(px, (px * 1.25).round());
        let mut probe = Buffer::new(
            &mut self.core.as_mut().expect("GpuCore 未挂载").font_system,
            metrics,
        );
        probe.set_size(None, None);
        probe.set_text(
            "M",
            &attrs_for(
                &config.font.family,
                config.font.weight,
                config.colors.foreground,
                false,
                false,
                false,
            ),
            Shaping::Advanced,
            None,
        );
        probe.shape_until_scroll(
            &mut self.core.as_mut().expect("GpuCore 未挂载").font_system,
            false,
        );
        let width = probe
            .layout_runs()
            .next()
            .map(|r| r.glyphs.iter().map(|g| g.w).sum::<f32>())
            .unwrap_or(px * 0.6)
            .round()
            .max(1.0);
        self.cell = (width, metrics.line_height);
        self.core.as_mut().expect("GpuCore 未挂载").font_key.family = config.font.family.clone();
        self.core.as_mut().expect("GpuCore 未挂载").font_key.weight = config.font.weight;
        self.core.as_mut().expect("GpuCore 未挂载").font_key.size_px = px;
        self.core.as_mut().expect("GpuCore 未挂载").font_metrics = metrics;
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .glyph_buffers
            .clear();
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .preedit_buffer
            .set_metrics(metrics);
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .preedit_buffer
            .set_monospace_width(Some(width));
    }

    /// 热重载单元格内边距(不重建字形系统;网格列数由调用方按新
    /// pad 重算,此处只更新坐标偏移)。
    pub fn update_padding(&mut self, padding: f32) {
        self.padding = padding;
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
            &self.core.as_mut().expect("GpuCore 未挂载").device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                format: self.format,
                width: w,
                height: h,
                present_mode: wgpu::PresentMode::AutoNoVsync,
                alpha_mode: self.alpha_mode,
                color_space: wgpu::SurfaceColorSpace::Auto,
                view_formats: vec![],
                desired_maximum_frame_latency: 2,
            },
        );

        self.apply_locals();
    }

    fn apply_locals(&mut self) {
        let locals = Locals {
            resolution: [self.size.0 as f32, self.size.1 as f32],
        };
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        core.queue
            .write_buffer(&core.locals_buf, 0, bytemuck::cast_slice(&[locals]));
        let set = core.glyph_sets.current.as_mut().expect("glyph 集未构建");
        set.1.viewport.update(
            &core.queue,
            glyphon::Resolution {
                width: self.size.0,
                height: self.size.1,
            },
        );
    }

    /// 用新的网格快照重建文本缓冲与四边形。渲染帧之前调用。
    pub fn update(&mut self, grid: &Grid) {
        self.tab_bar_rows = grid.tab_bar_rows;
        self.tab_panel_rows = grid.tab_panel_rows;
        self.rebuild_text(grid);
        self.rebuild_quads(grid);
        self.rebuild_images(grid);
    }

    /// 上传新图像纹理(架构 §6:数据身份 = Arc 指针,零哈希零
    /// 上传;缓存按当前可见集保留,删除/替换的图像纹理随帧释放)。
    fn rebuild_images(&mut self, grid: &Grid) {
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        let images: Vec<_> = core.wallpaper.iter().chain(grid.images.iter()).collect();
        self.current_images = images
            .iter()
            .copied()
            .enumerate()
            .map(|(index, img)| {
                ImageDraw::new(
                    img,
                    core.wallpaper.is_some() && index == 0,
                    self.cell,
                    self.padding,
                    self.size,
                )
            })
            .collect();
        // 只保留本帧可见图像的缓存项(删除/替换 → 旧纹理随帧释放)。
        let visible: std::collections::HashSet<(u32, usize)> =
            self.current_images.iter().map(|image| image.key).collect();
        core.img_cache.retain(|k, _| visible.contains(k));
        for img in images {
            let key = (
                img.id,
                std::sync::Arc::as_ptr(&img.rgba) as *const () as usize,
            );
            if core.img_cache.contains_key(&key) {
                continue;
            }
            if core.img_cache.len() > 64 {
                // 缓存上限:移除不在可见集里的最旧项(可见集已保留)。
                let keys: Vec<(u32, usize)> = core.img_cache.keys().copied().take(32).collect();
                for k in keys {
                    if !visible.contains(&k) {
                        core.img_cache.remove(&k);
                    }
                }
            }
            let texture = core.device.create_texture(&wgpu::TextureDescriptor {
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
            core.queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &img.rgba[..],
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
            let set = core.glyph_sets.current.as_ref().expect("glyph 集未构建");
            let bind = core.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tay kitty image bind"),
                layout: &set.1.img_pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: core.locals_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&core.img_sampler),
                    },
                ],
            });
            let inst = core.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tay image inst"),
                size: 16,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            core.img_cache
                .insert(key, (texture, bind, img.w, img.h, inst));
        }
    }

    /// Shape each distinct styled character once; placement always uses the
    /// terminal cell, never accumulated font advances (including fallback fonts).
    fn rebuild_text(&mut self, grid: &Grid) {
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        if core.glyph_buffers.len() > 4096 {
            core.glyph_buffers.clear();
        }
        self.glyph_draws = text_cells(grid, self.cursor_color);
        for draw in &self.glyph_draws {
            let key = draw.key();
            if core.glyph_buffers.contains_key(&key) {
                continue;
            }
            let mut buffer = Buffer::new(&mut core.font_system, core.font_metrics);
            buffer.set_wrap(glyphon::cosmic_text::Wrap::None);
            buffer.set_hinting(glyphon::cosmic_text::Hinting::Enabled);
            let mut cluster = draw.cell.c.to_string();
            if let Some(extra) = &draw.cell.zerowidth {
                cluster.push_str(extra);
            }
            buffer.set_text(
                &cluster,
                &attrs_for(
                    &core.font_key.family,
                    core.font_key.weight,
                    draw.cell.fg,
                    draw.cell.bold,
                    draw.cell.italic,
                    draw.cell.underline,
                ),
                Shaping::Advanced,
                None,
            );
            buffer.shape_until_scroll(&mut core.font_system, false);
            let mut ink: Option<GlyphInk> = None;
            for run in buffer.layout_runs() {
                for glyph in run.glyphs {
                    let physical = glyph.physical((0.0, 0.0), 1.0);
                    if let Some(image) = core
                        .swash_cache
                        .get_image(&mut core.font_system, physical.cache_key)
                    {
                        let p = image.placement;
                        if p.width == 0 || p.height == 0 {
                            continue;
                        }
                        let left = physical.x as f32 + p.left as f32;
                        let top = run.line_y.round() + physical.y as f32 - p.top as f32;
                        let extent = GlyphInk {
                            left,
                            top,
                            right: left + p.width as f32,
                            bottom: top + p.height as f32,
                        };
                        ink = Some(match ink {
                            Some(i) => GlyphInk {
                                left: i.left.min(extent.left),
                                top: i.top.min(extent.top),
                                right: i.right.max(extent.right),
                                bottom: i.bottom.max(extent.bottom),
                            },
                            None => extent,
                        });
                    }
                }
            }
            core.glyph_buffers.insert(key, CachedGlyph { buffer, ink });
        }
        self.preedit_right = grid.preedit.as_ref().and_then(|p| p.clip_right);
        self.preedit_pos = grid
            .preedit
            .as_ref()
            .filter(|p| p.row < grid.rows && p.col < grid.cols)
            .map(|p| (p.col, p.row));
        core.preedit_buffer.set_text(
            grid.preedit.as_ref().map(|p| p.text.as_str()).unwrap_or(""),
            &attrs_for(
                &core.font_key.family,
                core.font_key.weight,
                grid_default_fg(grid),
                false,
                false,
                true,
            ),
            Shaping::Advanced,
            None,
        );
        core.preedit_buffer
            .shape_until_scroll(&mut core.font_system, false);
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
                if bg == self.bg_default && !grid.opaque_background {
                    col += 1;
                    continue;
                }
                self.quads.push(Quad {
                    rect: [
                        start as f32 * cw + pad,
                        row as f32 * ch + pad,
                        (col - start + 1) as f32 * cw,
                        ch,
                    ],
                    color: to_linear(bg, self.core.as_mut().expect("GpuCore 未挂载").srgb()),
                });
                col += 1;
            }
        }

        // 圆角栏内面板遮罩及活动标签标记。
        for panel in &grid.chrome_panels {
            let x = pad + panel.start as f32 * cw;
            let y = pad + panel.row as f32 * ch;
            let w = (panel.end - panel.start) as f32 * cw;
            let ch = if panel.rows > 0 {
                ch * panel.rows as f32
            } else if panel.row == 0 {
                ch * grid.tab_panel_rows.max(1) as f32
            } else {
                ch
            };
            let inset = if panel.rows > 0 {
                1.0
            } else {
                (ch * 0.1).max(1.0)
            };
            let radius = if panel.rows > 0 {
                (self.cell.1 * 0.6).min(14.0)
            } else {
                ch * 0.28
            }
            .min(w * 0.5);
            let mask = to_linear(
                panel.background,
                self.core.as_mut().expect("GpuCore 未挂载").srgb(),
            );
            for rect in [[x, y, w, inset], [x, y + ch - inset, w, inset]] {
                self.quads.push(Quad { rect, color: mask });
            }
            for step in 0..radius.ceil() as usize {
                let dy = step as f32 + 0.5;
                let cut =
                    (radius - (radius * radius - (radius - dy).powi(2)).max(0.0).sqrt()).max(0.0);
                // 半圆角：左上与右下保留曲线，另两个角不裁剪。
                for rect in [
                    [x, y + inset + step as f32, cut, 1.0],
                    [x + w - cut, y + ch - inset - step as f32 - 1.0, cut, 1.0],
                ] {
                    self.quads.push(Quad { rect, color: mask });
                }
                if panel.corners == PanelCorners::All {
                    for rect in [
                        [x + w - cut, y + inset + step as f32, cut, 1.0],
                        [x, y + ch - inset - step as f32 - 1.0, cut, 1.0],
                    ] {
                        self.quads.push(Quad { rect, color: mask });
                    }
                }
            }
            if let Some(color) = panel.accent {
                self.quads.push(Quad {
                    rect: [
                        x + if panel.corners == PanelCorners::All {
                            radius
                        } else {
                            1.0
                        },
                        y + ch - inset - 2.0,
                        (w - radius
                            - if panel.corners == PanelCorners::All {
                                radius
                            } else {
                                1.0
                            })
                        .max(0.0),
                        2.0,
                    ],
                    color: to_linear(color, self.core.as_mut().expect("GpuCore 未挂载").srgb()),
                });
            }
        }
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
                    rect: [
                        x0 as f32 * cw + pad,
                        row as f32 * ch + pad,
                        (x1 - x0) as f32 * cw,
                        ch,
                    ],
                    color: to_linear(
                        self.selection_color,
                        self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                    ),
                });
            }
        }

        // 搜索命中高亮:与选区同构的色块,颜色独立(光标色
        // 淡染) —— 与鼠标/vi 选区同时出现时不混淆。
        if let Some(hit) = grid.search_hit {
            let (sx, sy) = hit.start;
            let (ex, ey) = hit.end;
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
                    rect: [
                        x0 as f32 * cw + pad,
                        row as f32 * ch + pad,
                        (x1 - x0) as f32 * cw,
                        ch,
                    ],
                    color: to_linear(
                        self.search_hit_color,
                        self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                    ),
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
                color: to_linear(
                    Rgb::new(0x00, 0x8b, 0x74),
                    self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                ),
            });
        }

        // 光标。
        if grid.cursor_shape != CursorShape::Hidden
            && grid.cursor.0 < grid.cols
            && grid.cursor.1 < grid.rows
            && grid.preedit.is_none()
        {
            let (cx, cy) = grid.cursor;
            let cursor_w = if cx + 1 < grid.cols && grid.cells[cy * grid.cols + cx + 1].wide_spacer
            {
                2.0 * cw
            } else {
                cw
            };
            let (x, y, w, h) = match grid.cursor_shape {
                CursorShape::Block => (cx as f32 * cw + pad, cy as f32 * ch + pad, cursor_w, ch),
                CursorShape::Underline => (
                    cx as f32 * cw + pad,
                    (cy + 1) as f32 * ch + pad - ch * 0.12,
                    cursor_w,
                    (ch * 0.08).max(2.0),
                ),
                CursorShape::Beam => (
                    cx as f32 * cw + pad,
                    cy as f32 * ch + pad,
                    (cw * 0.1).max(2.0),
                    ch,
                ),
                CursorShape::Hidden => unreachable!(),
            };
            self.quads.push(Quad {
                rect: [x, y, w, h],
                color: to_linear(
                    self.cursor_color,
                    self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                ),
            });
        }

        if let Some(geometry) = grid
            .scrollbar
            .and_then(|sb| sb.geometry(self.size.0 as f32, ch, pad))
        {
            self.quads.push(Quad {
                rect: [
                    geometry.x + geometry.width * 0.3,
                    geometry.top,
                    geometry.width * 0.4,
                    geometry.height,
                ],
                color: to_linear(
                    self.bg_default,
                    self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                ),
            });
            self.quads.push(Quad {
                rect: [
                    geometry.x + geometry.width * 0.3,
                    geometry.thumb_top,
                    geometry.width * 0.4,
                    geometry.thumb_height,
                ],
                color: to_linear(
                    self.cursor_color,
                    self.core.as_mut().expect("GpuCore 未挂载").srgb(),
                ),
            });
        }

        // Vector window controls: independent of fonts and font fallback.
        for icon in &grid.window_icons {
            let icon_height = if icon.row == 0 {
                ch * grid.tab_panel_rows.max(1) as f32
            } else {
                ch
            };
            let side = (ch * 0.65).round().max(8.0);
            let x = (pad + (icon.start + icon.end) as f32 * cw * 0.5 - side * 0.5).round();
            let y = (pad + icon.row as f32 * ch + (icon_height - side) * 0.5).round();
            let stroke = (ch * 0.05).round().max(1.0);
            let color = to_linear(
                grid.cells[icon.row * grid.cols + icon.start].fg,
                self.core.as_mut().expect("GpuCore 未挂载").srgb(),
            );
            let mut line = |x1: f32, y1: f32, x2: f32, y2: f32| {
                let count = ((x2 - x1).abs().max((y2 - y1).abs())).ceil().max(1.0) as usize;
                for i in 0..=count {
                    let t = i as f32 / count as f32;
                    self.quads.push(Quad {
                        rect: [
                            (x1 + (x2 - x1) * t).round(),
                            (y1 + (y2 - y1) * t).round(),
                            stroke,
                            stroke,
                        ],
                        color,
                    });
                }
            };
            let mut square = |x: f32, y: f32, size: f32| {
                line(x, y, x + size, y);
                line(x + size, y, x + size, y + size);
                line(x + size, y + size, x, y + size);
                line(x, y + size, x, y);
            };
            match icon.kind {
                WindowIconKind::Settings => {
                    let cx = x + side / 2.0;
                    let cy = y + side / 2.0;
                    let point = |i: usize| {
                        let angle = i as f32 * std::f32::consts::TAU / 64.0;
                        let radius = if i % 8 < 4 { side * 0.48 } else { side * 0.36 };
                        (cx + radius * angle.cos(), cy + radius * angle.sin())
                    };
                    for i in 0..64 {
                        let (ax, ay) = point(i);
                        let (bx, by) = point(i + 1);
                        line(ax, ay, bx, by);
                    }
                    for i in 0..32 {
                        let a = i as f32 * std::f32::consts::TAU / 32.0;
                        let b = (i + 1) as f32 * std::f32::consts::TAU / 32.0;
                        let r = side * 0.16;
                        line(
                            cx + r * a.cos(),
                            cy + r * a.sin(),
                            cx + r * b.cos(),
                            cy + r * b.sin(),
                        );
                    }
                }
                WindowIconKind::Maximize => square(x, y, side),
                WindowIconKind::Restore => {
                    square(x + side * 0.2, y, side * 0.8);
                    square(x, y + side * 0.2, side * 0.8);
                }
                WindowIconKind::Minimize => line(x, y + side * 0.7, x + side, y + side * 0.7),
                WindowIconKind::Close => {
                    line(x, y, x + side, y + side);
                    line(x + side, y, x, y + side);
                }
                WindowIconKind::Add => {
                    line(x, y + side * 0.5, x + side, y + side * 0.5);
                    line(x + side * 0.5, y, x + side * 0.5, y + side);
                }
            }
        }

        // 上传实例(超出容量则扩容)。
        let needed = (self.quads.len().max(1) * std::mem::size_of::<Quad>()) as u64;
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        if needed > core.instance_buf.size() {
            core.instance_buf = core.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("tay quad instances"),
                size: needed.next_power_of_two(),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        core.queue
            .write_buffer(&core.instance_buf, 0, bytemuck::cast_slice(&self.quads));
    }

    /// 渲染一帧到 `texture`(当前 surface 纹理)并 present。
    pub fn render(&mut self, texture: wgpu::SurfaceTexture, alpha: f32) -> Result<(), String> {
        let alpha = if self.alpha_mode == wgpu::CompositeAlphaMode::Opaque {
            1.0
        } else {
            alpha.clamp(0.0, 1.0)
        };
        let mut background = to_linear(
            self.bg_default,
            self.core.as_mut().expect("GpuCore 未挂载").srgb(),
        );
        if self.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied {
            for channel in &mut background[..3] {
                *channel *= alpha;
            }
        }
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            self.core()
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("tay frame"),
                });

        // 拷贝本帧绘制描述:areas 持 core 内缓冲的不可变借用,
        // 与 prepare 对 core 其他字段的可变借用需在同一个 core
        // 借用内做字段拆分(每帧几 KB 拷贝,可忽略)。
        let descs: Vec<(usize, usize, usize, char, GlyphKey)> = self
            .glyph_draws
            .iter()
            .map(|d| (d.col, d.row, d.width, d.cell.c, d.key()))
            .collect();
        let (cell, padding, tab_panel_rows, fg_default) = (
            self.cell,
            self.padding,
            self.tab_panel_rows,
            self.fg_default,
        );
        let (preedit_pos, preedit_right, size_w) =
            (self.preedit_pos, self.preedit_right, self.size.0);
        {
            let core = self.core.as_mut().expect("GpuCore 未挂载");
            let set = &mut core.glyph_sets.current.as_mut().expect("glyph 集未构建").1;
            let mut areas: Vec<TextArea<'_>> = Vec::new();
            for (col, row, width, c, key) in &descs {
                let left = padding + *col as f32 * cell.0;
                let top = padding
                    + *row as f32 * cell.1
                    + if *row == 0 {
                        tab_panel_rows.saturating_sub(1) as f32 * cell.1 * 0.5
                    } else {
                        0.0
                    };
                let cached = &core.glyph_buffers[key];
                let (dx, dy, scale) = fit_glyph(cached.ink, *c, *width, cell);
                areas.push(TextArea {
                    buffer: &cached.buffer,
                    left: left + dx,
                    top: top + dy,
                    scale,
                    bounds: TextBounds {
                        left: left.floor() as i32,
                        top: top.floor() as i32,
                        right: (left + *width as f32 * cell.0).ceil() as i32,
                        bottom: (top + cell.1).ceil() as i32,
                    },
                    default_color: fg_default,
                    custom_glyphs: &[],
                });
            }
            if let Some((col, row)) = preedit_pos {
                let left = padding + col as f32 * cell.0;
                let top = padding + row as f32 * cell.1;
                areas.push(TextArea {
                    buffer: &core.preedit_buffer,
                    left,
                    top,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: left.floor() as i32,
                        top: top.floor() as i32,
                        right: preedit_right
                            .map(|col| (padding + col as f32 * cell.0).ceil() as i32)
                            .unwrap_or(size_w as i32)
                            .min(size_w as i32),
                        bottom: (top + cell.1).ceil() as i32,
                    },
                    default_color: fg_default,
                    custom_glyphs: &[],
                });
            }
            set.text
                .prepare(
                    &core.device,
                    &core.queue,
                    &mut core.font_system,
                    &mut set.atlas,
                    &set.viewport,
                    areas,
                    &mut core.swash_cache,
                )
                .map_err(|e| format!("文本准备失败: {e}"))?;
        }

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tay pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: background[0] as f64,
                            g: background[1] as f64,
                            b: background[2] as f64,
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
            self.draw_images(&mut pass, false, true);
            if !self.quads.is_empty() {
                self.draw_quads(&mut pass);
            }
            // 图像:z<=0 在文本之下。
            self.draw_images(&mut pass, false, false);
            {
                let core = self.core.as_mut().expect("GpuCore 未挂载");
                let set = core.glyph_set_mut();
                set.text
                    .render(&set.atlas, &set.viewport, &mut pass)
                    .map_err(|e| format!("文本渲染失败: {e}"))?;
            }
            // 图像:z>0 在文本之上。
            self.draw_images(&mut pass, true, false);
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

        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .queue
            .submit(Some(encoder.finish()));
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .queue
            .present(texture);
        Ok(())
    }

    /// 画 kitty 图像(`above` = 文本之上)。
    /// 四边形实例(单元格底色/选区/光标)绘制:core 借用与 self 方法
    /// 调用互斥,单独成方法。
    fn draw_quads(&mut self, pass: &mut wgpu::RenderPass<'_>) {
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        pass.set_pipeline(core.bg_pipeline());
        pass.set_bind_group(0, &core.locals_bind, &[]);
        pass.set_vertex_buffer(0, core.unit_quad.slice(..));
        pass.set_vertex_buffer(1, core.instance_buf.slice(..));
        pass.draw(0..6, 0..self.quads.len() as u32);
    }

    fn draw_images(&mut self, pass: &mut wgpu::RenderPass<'_>, above: bool, wallpaper: bool) {
        let core = self.core.as_mut().expect("GpuCore 未挂载");
        for image in self
            .current_images
            .iter()
            .filter(|image| image.in_pass(above, wallpaper))
        {
            let key = image.key;
            let rect = image.rect;
            let [x, y, w, h] = image.clip;
            if w == 0 || h == 0 {
                continue;
            }
            pass.set_scissor_rect(x, y, w, h);
            let Some((_, bind, _, _, inst)) = core.img_cache.get(&key) else {
                continue;
            };
            core.queue
                .write_buffer(inst, 0, bytemuck::cast_slice(&[rect]));
            pass.set_pipeline(core.img_pipeline());
            pass.set_bind_group(0, bind, &[]);
            pass.set_vertex_buffer(0, core.uv_quad.slice(..));
            pass.set_vertex_buffer(1, inst.slice(..));
            pass.draw(0..6, 0..1);
        }
        pass.set_scissor_rect(0, 0, self.size.0, self.size.1);
    }

    /// 把当前帧纹理读回并保存为 PNG(阻塞)。
    fn capture(&mut self, mut encoder: wgpu::CommandEncoder, texture: &wgpu::Texture, path: &str) {
        let size = texture.size();
        let (w, h) = (size.width, size.height);
        // BGRA 每行 256 字节对齐。
        let bpr = (w * 4).next_multiple_of(256);
        let buf = self
            .core
            .as_mut()
            .expect("GpuCore 未挂载")
            .device
            .create_buffer(&wgpu::BufferDescriptor {
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
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .queue
            .submit(Some(encoder.finish()));
        let slice = buf.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.core
            .as_mut()
            .expect("GpuCore 未挂载")
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .ok();
        if rx.recv().is_err() {
            log::error!("截图读回失败");
            return;
        }
        let data = slice
            .get_mapped_range()
            .map_err(|e| log::error!("映射截图缓冲失败: {e}"))
            .ok();
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
                let alpha = data[src + x * 4 + 3];
                rgba[dst + x * 4 + 3] = alpha;
                if self.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied
                    && alpha > 0
                    && alpha < 255
                {
                    for channel in 0..3 {
                        rgba[dst + x * 4 + channel] = ((rgba[dst + x * 4 + channel] as u32 * 255)
                            / alpha as u32)
                            .min(255) as u8;
                    }
                }
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

/// 协商 surface 格式/alpha 并完成 configure。
fn negotiate_configure(
    surface: &wgpu::Surface,
    adapter: &wgpu::Adapter,
    device: &wgpu::Device,
    size: (u32, u32),
) -> Result<(wgpu::TextureFormat, wgpu::CompositeAlphaMode), String> {
    let caps = surface.get_capabilities(adapter);
    // 优先选 sRGB 格式(与 glyphon 的默认一致),否则取第一个。
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .or_else(|| caps.formats.first().copied())
        .ok_or("surface 无可用格式")?;
    let alpha_mode = [
        wgpu::CompositeAlphaMode::PreMultiplied,
        wgpu::CompositeAlphaMode::PostMultiplied,
    ]
    .into_iter()
    .find(|m| caps.alpha_modes.contains(m))
    .unwrap_or(wgpu::CompositeAlphaMode::Opaque);
    log::info!("Surface alpha mode: {alpha_mode:?}");
    surface.configure(
        device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: size.0.max(1),
            height: size.1.max(1),
            present_mode: wgpu::PresentMode::AutoNoVsync,
            alpha_mode,
            color_space: wgpu::SurfaceColorSpace::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
    Ok((format, alpha_mode))
}

fn font_key_of(config: &Config, scale_factor: f64) -> FontKey {
    FontKey {
        family: config.font.family.clone(),
        weight: config.font.weight,
        size_px: config.font.size * 96.0 / 72.0 * scale_factor as f32,
    }
}

/// 在给定 FontSystem 上 probe "M" 测量单元格尺寸(probe 廉价,复用字体栈时也执行)。
fn probe_cell(
    font_system: &mut FontSystem,
    config: &Config,
    scale_factor: f64,
    format: Option<wgpu::TextureFormat>,
) -> (f32, f32) {
    let font_size_px = config.font.size * 96.0 / 72.0 * scale_factor as f32;
    let metrics = Metrics::new(font_size_px, (font_size_px * 1.25).round());
    let mut probe = Buffer::new(font_system, metrics);
    probe.set_size(None, None);
    let probe_attrs = attrs_for(
        &config.font.family,
        config.font.weight,
        config.colors.foreground,
        false,
        false,
        false,
    );
    probe.set_text("M", &probe_attrs, Shaping::Advanced, None);
    probe.shape_until_scroll(font_system, false);
    let mut cell_w = font_size_px * 0.6; // 兜底:等宽近似
    let mut cell_h = metrics.line_height;
    if let Some(run) = probe.layout_runs().next() {
        cell_w = run.glyphs.iter().map(|g| g.w).sum::<f32>().round().max(1.0);
        cell_h = run.line_height.max(1.0);
    }
    if let Some(format) = format {
        log::info!(
            "字体 {} @ {font_size_px:.1}px → 单元格 {cell_w:.2}×{cell_h:.2}px,surface 格式 {format:?}",
            config.font.family
        );
    }
    (cell_w, cell_h)
}

/// 全新字体栈(首次初始化 / 字体键变化时)。
fn build_fonts(
    config: &Config,
    scale_factor: f64,
) -> (FontSystem, SwashCache, Metrics, f32, (f32, f32)) {
    log::info!("字体栈构建(FontSystem::new,首次或字体键变化)");
    let font_size_px = config.font.size * 96.0 / 72.0 * scale_factor as f32;
    let mut font_system = FontSystem::new();
    let (cell_w, cell_h) = probe_cell(&mut font_system, config, scale_factor, None);
    let swash_cache = SwashCache::new();
    (
        font_system,
        swash_cache,
        Metrics::new(font_size_px, cell_h),
        font_size_px,
        (cell_w, cell_h),
    )
}

/// 按 surface format 构建 glyphon 集与两条自绘管线(device 级资源,format 绑定)。
fn build_glyph_set(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
) -> Result<GlyphSet, String> {
    // —— glyphon ——
    let cache = Cache::new(device);
    let mut atlas = TextAtlas::new(device, queue, &cache, format);
    let text = TextRenderer::new(&mut atlas, device, wgpu::MultisampleState::default(), None);
    let viewport = Viewport::new(device, &cache);

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

    Ok(GlyphSet {
        cache,
        atlas,
        text,
        viewport,
        bg_pipeline,
        img_pipeline,
        srgb: format.is_srgb(),
    })
}

/// 首次初始化:Adapter/Device/字体/glyph 集/统一缓冲(Instance 由调用方持有)。
fn build_fresh(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface,
    config: &Config,
    size: (u32, u32),
    scale_factor: f64,
) -> Result<(GpuCore, wgpu::TextureFormat, wgpu::CompositeAlphaMode), String> {
    let (adapter, device, queue) = crate::gpu_core::request_adapter_device(instance, surface)?;
    let (format, alpha_mode) = negotiate_configure(surface, &adapter, &device, size)?;
    let (mut font_system, swash_cache, metrics, size_px, cell) = build_fonts(config, scale_factor);
    let glyph_set = build_glyph_set(&device, &queue, format)?;
    let preedit_buffer = Buffer::new(&mut font_system, Metrics::new(size_px, cell.1));
    let (uv_quad, img_sampler, unit_quad, instance_buf, locals_buf, locals_bind) =
        build_buffers(&device);

    device.on_uncaptured_error(std::sync::Arc::new(|e| {
        log::error!("wgpu 校验错误: {e:?}");
    }));

    let core = GpuCore {
        adapter: Some(adapter),
        device,
        queue,
        font_system,
        swash_cache,
        glyph_sets: FormatCache {
            current: Some((format, glyph_set)),
        },
        glyph_buffers: Default::default(),
        font_metrics: metrics,
        preedit_buffer,
        font_key: FontKey {
            family: config.font.family.clone(),
            weight: config.font.weight,
            size_px,
        },
        wallpaper: None,
        wallpaper_config: (String::new(), -1.0, -1.0),
        background_checked: None,
        img_cache: Default::default(),
        uv_quad,
        img_sampler,
        unit_quad,
        instance_buf,
        locals_buf,
        locals_bind,
    };
    Ok((core, format, alpha_mode))
}

/// device 级统一缓冲与采样器(首次构建与混合 GPU 重建共用)。
fn build_buffers(
    device: &wgpu::Device,
) -> (
    wgpu::Buffer,
    wgpu::Sampler,
    wgpu::Buffer,
    wgpu::Buffer,
    wgpu::Buffer,
    wgpu::BindGroup,
) {
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
    let locals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("tay locals bind"),
        layout: &locals_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: locals_buf.as_entire_binding(),
        }],
    });
    (
        uv_quad,
        img_sampler,
        unit_quad,
        instance_buf,
        locals_buf,
        locals_bind,
    )
}

/// 网格首格的默认前景色(空行 span 用)。
fn grid_default_fg(grid: &Grid) -> Rgb {
    grid.cells
        .first()
        .map(|c| c.fg)
        .unwrap_or(Rgb::new(0xd4, 0xd4, 0xd4))
}

#[cfg(test)]
mod tests {
    #[test]
    fn minimum_kitty_z_keeps_cell_geometry_and_terminal_pass() {
        let img = super::RenderImage {
            id: 42,
            clip: None,
            row: 2,
            col: 3,
            cols: 4,
            rows: 5,
            w: 2,
            h: 2,
            z: i32::MIN,
            rgba: vec![255; 16].into(),
        };
        let terminal = super::ImageDraw::new(&img, false, (10.0, 20.0), 3.0, (800, 600));
        assert_eq!(terminal.rect, [33.0, 43.0, 40.0, 100.0]);
        assert!(terminal.in_pass(false, false));
        assert!(!terminal.in_pass(false, true));
        assert!(!terminal.in_pass(true, false));
        let wallpaper = super::ImageDraw::new(&img, true, (10.0, 20.0), 3.0, (800, 600));
        assert_eq!(wallpaper.rect, [0.0, -100.0, 800.0, 800.0]);
        assert!(wallpaper.in_pass(false, true));
        assert!(!wallpaper.in_pass(false, false));
    }

    use super::*;
    #[test]
    fn scrollbar_geometry_and_drag_agree_at_both_ends() {
        for ch in [12.0, 24.0, 48.0] {
            for position in [0.0, 0.5, 1.0] {
                let g = Scrollbar {
                    right: None,
                    top: 3.0,
                    extent: 20.0,
                    position,
                    size: 0.01,
                }
                .geometry(800.0, ch, 10.0)
                .unwrap();
                assert!(
                    g.thumb_top >= g.top
                        && g.thumb_top + g.thumb_height <= g.top + g.height + 0.001
                );
                let grab = g.thumb_height / 2.0;
                assert_eq!(
                    g.offset(g.thumb_top + grab, grab, 1000),
                    (position * 1000.0).round() as usize
                );
                assert_eq!(g.offset(-100.0, grab, 1000), 1000);
                assert_eq!(g.offset(10000.0, grab, 1000), 0);
                assert!(g.contains(799.0, g.top));
                assert!(!g.contains(g.x - 1.0, g.top));
            }
        }
        let tiny = Scrollbar {
            right: None,
            top: 1.0,
            extent: 1.0,
            position: 0.0,
            size: 0.001,
        }
        .geometry(800.0, 12.0, 0.0)
        .unwrap();
        assert!(tiny.thumb_height < tiny.height);
        assert_eq!(tiny.offset(-10.0, 0.0, 1000), 1000);
        assert_eq!(tiny.offset(1000.0, 0.0, 1000), 0);
        assert!(
            Scrollbar {
                right: None,
                top: 0.0,
                extent: 20.0,
                position: 0.0,
                size: 1.0
            }
            .geometry(800.0, 20.0, 0.0)
            .is_none()
        );
    }
    #[test]
    fn symbol_ink_fits_without_moving_terminal_column_origins() {
        let icon = GlyphInk {
            left: 0.0,
            top: 5.0,
            right: 34.0,
            bottom: 32.0,
        };
        let (x, y, scale) = fit_glyph(Some(icon), '\u{f120}', 1, (19.0, 40.0));
        assert!(scale < 1.0);
        assert!(x + icon.left * scale >= 0.0);
        assert!(x + icon.right * scale <= 19.001);
        assert!(y + icon.top * scale >= 0.0 && y + icon.bottom * scale <= 40.001);
        assert_eq!(
            fit_glyph(Some(icon), '\u{f120}', 2, (19.0, 40.0)),
            (0.0, 0.0, 1.0)
        );
        let italic = GlyphInk {
            left: -3.0,
            top: 6.0,
            right: 14.0,
            bottom: 32.0,
        };
        assert_eq!(
            fit_glyph(Some(italic), 'j', 1, (19.0, 40.0)),
            (3.0, 0.0, 1.0)
        );
        let powerline = GlyphInk {
            left: -2.0,
            top: -2.0,
            right: 20.0,
            bottom: 41.0,
        };
        assert_eq!(
            fit_glyph(Some(powerline), '\u{e0b0}', 1, (19.0, 40.0)),
            (0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn clusters_survive_draw_cache_and_icons_only_use_empty_neighbor_cells() {
        let cell = Cell {
            c: 'e',
            zerowidth: Some("\u{301}".into()),
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
        };
        let mut grid = Grid {
            cols: 4,
            rows: 1,
            cells: vec![cell; 4],
            cursor: (3, 0),
            cursor_shape: CursorShape::Hidden,
            selection: None,
            search_hit: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        grid.cells[1].zerowidth = Some("\u{308}".into());
        let draws = text_cells(&grid, Rgb(0, 255, 0));
        assert_ne!(draws[0].key(), draws[1].key());
        assert_eq!(draws[0].cell.zerowidth.as_deref(), Some("\u{301}"));
        grid.cells[0].c = '\u{f120}';
        grid.cells[0].zerowidth = None;
        grid.cells[1].c = ' ';
        grid.cells[1].zerowidth = None;
        assert_eq!(text_cells(&grid, Rgb(0, 255, 0))[0].width, 2);
        grid.cells[1].c = 'a';
        assert_eq!(text_cells(&grid, Rgb(0, 255, 0))[0].width, 1);
        grid.cells[1].c = ' ';
        grid.cursor = (1, 0);
        assert_eq!(text_cells(&grid, Rgb(0, 255, 0))[0].width, 1);
        grid.cursor = (3, 0);
        grid.cells[1].bg = Rgb(255, 0, 0);
        assert_eq!(text_cells(&grid, Rgb(0, 255, 0))[0].width, 1);
    }

    #[test]
    fn text_and_cursor_share_grid_positions_with_wide_and_bold_cells() {
        let cell = Cell {
            c: 'i',
            fg: Rgb(255, 255, 255),
            bg: Rgb(0, 0, 0),
            bold: false,
            italic: false,
            underline: false,
            wide_spacer: false,
            zerowidth: None,
        };
        let mut cells = vec![cell; 8];
        cells[2].c = '中';
        cells[3].c = ' ';
        cells[3].wide_spacer = true;
        cells[4].bold = true;
        let mut grid = Grid {
            cols: 8,
            rows: 1,
            cells,
            cursor: (4, 0),
            cursor_shape: CursorShape::Block,
            selection: None,
            search_hit: None,
            preedit: None,
            badges: vec![],
            overlays: vec![],
            images: vec![],
            window_icons: vec![],
            chrome_panels: vec![],
            tab_bar_rows: 0,
            tab_panel_rows: 0,
            opaque_background: false,
            scrollbar: None,
        };
        let draws = text_cells(&grid, Rgb(0, 160, 128));
        assert_eq!(
            draws.iter().map(|d| d.col).collect::<Vec<_>>(),
            vec![0, 1, 2, 4, 5, 6, 7]
        );
        assert_eq!(draws[2].width, 2);
        assert_eq!(draws[3].cell.fg, contrast_text(Rgb(0, 160, 128)));
        grid.preedit = Some(Preedit {
            text: "你好".into(),
            col: 2,
            row: 0,
            cursor_bytes: None,
            clip_right: None,
        });
        let draws = text_cells(&grid, Rgb(0, 160, 128));
        assert_eq!(
            draws.iter().map(|d| d.col).collect::<Vec<_>>(),
            vec![0, 1, 6, 7]
        );
    }
}
