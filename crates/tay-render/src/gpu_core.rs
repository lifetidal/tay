//! GpuCore:跨 surface 重建常驻的 GPU 资源与纯决策逻辑。

use crate::{CachedGlyph, GlyphKey, ImageCache, RenderImage};
use glyphon::{FontSystem, Metrics, SwashCache, TextAtlas, TextRenderer, Viewport};

/// 字体栈重建键:字体族/字重/字号任一变化即重载字体并清空 glyph 缓存。
#[derive(Debug, Clone, PartialEq)]
pub struct FontKey {
    pub family: String,
    pub weight: u16,
    pub size_px: f32,
}

impl std::hash::Hash for FontKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.family.hash(state);
        self.weight.hash(state);
        self.size_px.to_bits().hash(state);
    }
}

/// 按 surface format 缓存的资源集的取舍决策。
pub enum CacheDecision {
    /// 同 format,直接复用。
    Reuse,
    /// 不同 format:重建并丢弃旧集(只保留最近一个 format)。
    RebuildAndDrop,
    /// 缓存为空:首次构建。
    RebuildFirst,
}

/// 单槽 format 缓存:同一时刻只保留一个 format 的资源集。
pub struct FormatCache<T> {
    pub current: Option<(wgpu::TextureFormat, T)>,
}

impl<T> Default for FormatCache<T> {
    fn default() -> Self {
        Self { current: None }
    }
}

impl<T> FormatCache<T> {
    pub fn decide(&mut self, format: wgpu::TextureFormat) -> CacheDecision {
        match &self.current {
            Some((f, _)) if *f == format => CacheDecision::Reuse,
            Some(_) => CacheDecision::RebuildAndDrop,
            None => CacheDecision::RebuildFirst,
        }
    }
}

/// 按 surface format 缓存的可重建资源集(glyphon 图集/管线等)。
pub struct GlyphSet {
    pub cache: glyphon::Cache,
    pub atlas: TextAtlas,
    pub text: TextRenderer,
    pub viewport: Viewport,
    pub bg_pipeline: wgpu::RenderPipeline,
    pub img_pipeline: wgpu::RenderPipeline,
    pub srgb: bool,
}

impl GlyphSet {
    /// 字段级借用拆分:调用方常在持有 atlas/text/viewport 的
    /// 不可变/可变借用的同时访问 GpuCore 的其他字段。
    pub fn parts(&mut self) -> (&mut TextAtlas, &mut TextRenderer, &mut Viewport) {
        (&mut self.atlas, &mut self.text, &mut self.viewport)
    }
}

/// 缓存帧在新 surface 内的居中 aspect-fit 矩形 (x, y, w, h)。
/// 任一维为 0 → None(调用方跳过 blit)。
pub fn cached_fit_rect(src: (u32, u32), dst: (u32, u32)) -> Option<(f32, f32, f32, f32)> {
    if src.0 == 0 || src.1 == 0 || dst.0 == 0 || dst.1 == 0 {
        return None;
    }
    let scale = ((dst.0 as f32) / (src.0 as f32)).min((dst.1 as f32) / (src.1 as f32));
    let w = src.0 as f32 * scale;
    let h = src.1 as f32 * scale;
    Some(((dst.0 as f32 - w) * 0.5, (dst.1 as f32 - h) * 0.5, w, h))
}

/// 缓存帧:收起时捕获的最后一帧 + 其上屏所需资源(有界 1 张)。
pub struct CachedFrame {
    pub texture: wgpu::Texture,
    pub size: (u32, u32),
    /// 以缓存纹理为源的 bind group(remap 后 blit 用)。
    pub bind: wgpu::BindGroup,
    /// blit 实例缓冲(rect 写入)。
    pub inst: wgpu::Buffer,
}

/// 跨 remap 常驻的 device 级资源:Adapter/Device/Queue、
/// 字体栈、按 format 缓存的 glyphon 集、图像缓存与统一缓冲。
/// Instance 由调用方(TayApp)持有 —— surface 与 device 必须来自
/// 同一个 Instance,否则 wgpu 查不到 surface 资源。
pub struct GpuCore {
    pub adapter: Option<wgpu::Adapter>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub font_system: FontSystem,
    pub swash_cache: SwashCache,
    pub glyph_sets: FormatCache<GlyphSet>,
    pub(crate) glyph_buffers: std::collections::HashMap<GlyphKey, CachedGlyph>,
    pub font_metrics: Metrics,
    pub preedit_buffer: glyphon::Buffer,
    pub font_key: FontKey,
    pub wallpaper: Option<RenderImage>,
    pub wallpaper_config: (String, f32, f32),
    pub background_checked: Option<std::time::Instant>,
    pub img_cache: ImageCache,
    pub uv_quad: wgpu::Buffer,
    pub img_sampler: wgpu::Sampler,
    pub unit_quad: wgpu::Buffer,
    pub instance_buf: wgpu::Buffer,
    pub locals_buf: wgpu::Buffer,
    pub locals_bind: wgpu::BindGroup,
    pub cached_frame: Option<CachedFrame>,
}

/// 请求 adapter/device/queue(ensure_device 与首次构建共用)。
/// 错误处理器在此统一注册:device 无论哪条路径创建,都必须有
/// uncaptured error 兜底(wgpu 30 默认把未捕获错误升级为 panic)。
pub fn request_adapter_device(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), String> {
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::None,
        force_fallback_adapter: false,
        compatible_surface: Some(surface),
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
    device.on_uncaptured_error(std::sync::Arc::new(|e| {
        log::error!("wgpu 校验错误: {e:?}");
    }));
    Ok((adapter, device, queue))
}

impl GpuCore {
    /// 保证 adapter/device 可用:已有 adapter 且新 surface 可呈现则复用;
    /// 否则(首建或混合 GPU 迁移)为该 surface 重建 adapter+device,
    /// 并清空 format 缓存(旧 device 的资源随之失效)。
    /// 返回 true 表示 device 已重建 —— 调用方必须重建全部 device 级缓冲
    /// (locals_buf/quad/图像缓存/壁纸),否则跨 device 校验 panic。
    pub fn ensure_device(
        &mut self,
        instance: &wgpu::Instance,
        surface: &wgpu::Surface,
    ) -> Result<bool, String> {
        if let Some(adapter) = &self.adapter {
            // TAY_FORCE_DEVICE_REBUILD=1:真机验收钩子 —— 强制走混合
            // GPU 重建路径(本机单 GPU 无法自然触发,借此验证
            // 「重建后资源一致性」这一整条分支)。
            let force = std::env::var("TAY_FORCE_DEVICE_REBUILD").is_ok_and(|v| v == "1");
            if !force {
                let caps = surface.get_capabilities(adapter);
                log::info!(
                    "适配器能力检查: formats={} alpha_modes={}",
                    caps.formats.len(),
                    caps.alpha_modes.len()
                );
                if !caps.formats.is_empty() {
                    return Ok(false);
                }
            }
            log::warn!("适配器与新 surface 不兼容,重建 device(混合 GPU 路径)");
        }
        let (adapter, device, queue) = request_adapter_device(instance, surface)?;
        self.adapter = Some(adapter);
        self.device = device;
        self.queue = queue;
        self.glyph_sets = FormatCache::default();
        Ok(true)
    }

    pub fn glyph_set(&self) -> &GlyphSet {
        &self.glyph_sets.current.as_ref().expect("glyph 集未构建").1
    }

    pub fn glyph_set_mut(&mut self) -> &mut GlyphSet {
        &mut self.glyph_sets.current.as_mut().expect("glyph 集未构建").1
    }

    pub fn srgb(&self) -> bool {
        self.glyph_set().srgb
    }

    pub fn bg_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.glyph_set().bg_pipeline
    }

    pub fn img_pipeline(&self) -> &wgpu::RenderPipeline {
        &self.glyph_set().img_pipeline
    }

    pub fn atlas_mut(&mut self) -> &mut TextAtlas {
        &mut self.glyph_set_mut().atlas
    }

    pub fn text_mut(&mut self) -> &mut TextRenderer {
        &mut self.glyph_set_mut().text
    }

    pub fn viewport_mut(&mut self) -> &mut Viewport {
        &mut self.glyph_set_mut().viewport
    }

    /// 把当前交换链帧拷入常驻缓存纹理(有界 1 张;尺寸变化时重建)。
    pub fn capture_frame(
        &mut self,
        src: &wgpu::Texture,
        encoder: &mut wgpu::CommandEncoder,
        size: (u32, u32),
    ) {
        if self.cached_frame.as_ref().map(|c| c.size) != Some(size) {
            let format = self.glyph_sets.current.as_ref().expect("glyph 集未构建").0;
            let texture = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("tay cached frame"),
                size: wgpu::Extent3d {
                    width: size.0.max(1),
                    height: size.1.max(1),
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("tay cached frame bind"),
                layout: &self
                    .glyph_sets
                    .current
                    .as_ref()
                    .expect("glyph 集未构建")
                    .1
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
                label: Some("tay cached frame inst"),
                size: 16,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.cached_frame = Some(CachedFrame {
                texture,
                size,
                bind,
                inst,
            });
        }
        if let Some(cached) = &self.cached_frame {
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: src,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyTextureInfo {
                    texture: &cached.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d {
                    width: size.0.max(1),
                    height: size.1.max(1),
                    depth_or_array_layers: 1,
                },
            );
            log::info!("缓存帧已捕获 {w}x{h}", w = size.0, h = size.1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DropT(Arc<AtomicUsize>);

    impl Drop for DropT {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    const FMT_A: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;
    const FMT_B: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    #[test]
    fn format_cache_reuses_same_format() {
        let mut c = FormatCache::<()> {
            current: Some((FMT_A, ())),
        };
        assert!(matches!(c.decide(FMT_A), CacheDecision::Reuse));
    }

    #[test]
    fn format_cache_evicts_old() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut c = FormatCache {
            current: Some((FMT_A, DropT(drops.clone()))),
        };
        assert!(matches!(c.decide(FMT_B), CacheDecision::RebuildAndDrop));
        // 执行 decide 决定的替换:旧值被丢弃。
        c.current = Some((FMT_B, DropT(drops.clone())));
        assert_eq!(drops.load(Ordering::SeqCst), 1, "旧 format 集必须被丢弃");
    }

    #[test]
    fn format_cache_first_build() {
        let mut c = FormatCache::<()>::default();
        assert!(matches!(c.decide(FMT_A), CacheDecision::RebuildFirst));
    }

    #[test]
    fn font_key_equality() {
        let a = FontKey {
            family: "JetBrainsMono Nerd Font".into(),
            weight: 700,
            size_px: 32.0,
        };
        let b = FontKey {
            family: "JetBrainsMono Nerd Font".into(),
            weight: 700,
            size_px: 32.0,
        };
        let c = FontKey {
            family: "JetBrainsMono Nerd Font".into(),
            weight: 400,
            size_px: 32.0,
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn fit_rect_same_size_fills_dst() {
        assert_eq!(
            cached_fit_rect((400, 400), (400, 400)),
            Some((0.0, 0.0, 400.0, 400.0))
        );
    }

    #[test]
    fn fit_rect_narrower_src_centers_horizontally() {
        // src 200×100 → dst 400×400:等比放大到 400×200,上下留边。
        assert_eq!(
            cached_fit_rect((200, 100), (400, 400)),
            Some((0.0, 100.0, 400.0, 200.0))
        );
    }

    #[test]
    fn fit_rect_wider_src_centers_vertically() {
        // src 400×100 → dst 100×400:scale 0.25 → 100×25,上下留边。
        assert_eq!(
            cached_fit_rect((400, 100), (100, 400)),
            Some((0.0, 187.5, 100.0, 25.0))
        );
    }

    #[test]
    fn fit_rect_zero_dimension_is_none() {
        assert_eq!(cached_fit_rect((0, 100), (400, 400)), None);
        assert_eq!(cached_fit_rect((200, 0), (400, 400)), None);
    }
}
