//! One process-wide decoder; UI and surface rebuilds only exchange Arc handles.
use crate::RenderImage;
use std::{
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::Duration,
};
type Key = (String, u32, u32);
type Shared = Arc<Mutex<Option<(Key, RenderImage)>>>;
/// 文件身份(dev/ino/len/mtime/mtime_nsec/ctime/ctime_nsec)。
type Version = (u64, u64, u64, i64, i64, i64, i64);
struct Loader {
    tx: mpsc::SyncSender<Key>,
    cache: Shared,
    /// 最近一次解码失败原因(设置面板背景页读取;成功解码后清空)。
    error: Arc<Mutex<Option<String>>>,
}
static LOADER: OnceLock<Loader> = OnceLock::new();

fn path(raw: &str) -> PathBuf {
    raw.strip_prefix("~/")
        .and_then(|rest| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(rest)))
        .unwrap_or_else(|| raw.into())
}

/// 读盘 + 全量解码(尺寸受限);结果只含原始 RGBA,不掺透明度/亮度。
fn decode_base(path: &std::path::Path) -> Result<(u32, u32, Arc<[u8]>), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("背景必须是普通图片文件".into());
    }
    let mut reader = image::ImageReader::open(path).map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let pixels = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = (pixels.width(), pixels.height());
    Ok((w, h, pixels.into_raw().into()))
}

/// 在基础像素上应用透明度/亮度(滑条每步只走这里,不重新解码)。
fn apply_pixels(w: u32, h: u32, base: &[u8], opacity: f32, brightness: f32) -> RenderImage {
    let mut pixels: Vec<u8> = base.to_vec();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel[3] = (pixel[3] as f32 * opacity) as u8;
        for channel in &mut pixel[..3] {
            *channel = (*channel as f32 * brightness) as u8;
        }
    }
    RenderImage {
        id: u32::MAX,
        clip: None,
        row: 0,
        col: 0,
        cols: 1,
        rows: 1,
        w,
        h,
        z: 0,
        rgba: pixels.into(),
    }
}
fn loader() -> &'static Loader {
    LOADER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Key>(1);
        let cache: Shared = Arc::new(Mutex::new(None));
        let output = cache.clone();
        let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let error_out = error.clone();
        std::thread::Builder::new()
            .name("wallpaper".into())
            .spawn(move || {
                let mut current = None;
                let mut loaded = None;
                // 基础像素缓存:同一文件版本只解码一次,透明度/亮度
                // 变化(滑条每步)只重算像素 —— 否则每步都全量读盘
                // + 解码(4K 图几十上百 ms)。
                type BaseCache = (PathBuf, Option<Version>, u32, u32, Arc<[u8]>);
                let mut base: Option<BaseCache> = None;
                // 最近一次解码失败的身份 + 时间:同一身份在退避窗口内
                // 不重试(坏图不空转);文件被替换/修改后 identity 变化
                // 立即重试,瞬时失败(解码 OOM、短读等)在退避后自动恢复。
                const RETRY_BACKOFF: Duration = Duration::from_secs(30);
                let mut failed: Option<((Key, Option<Version>), std::time::Instant)> = None;
                loop {
                    match rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(key) => {
                            current = Some(key);
                            while let Ok(key) = rx.try_recv() {
                                current = Some(key);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let Some(key) = current.as_ref() else {
                        continue;
                    };
                    if key.0.is_empty() {
                        *output.lock().unwrap() = None;
                        *error_out.lock().unwrap() = None;
                        loaded = None;
                        failed = None;
                        continue;
                    }
                    let path = path(&key.0);
                    let version = std::fs::metadata(&path).ok().map(|m| {
                        (
                            m.dev(),
                            m.ino(),
                            m.len(),
                            m.mtime(),
                            m.mtime_nsec(),
                            m.ctime(),
                            m.ctime_nsec(),
                        )
                    });
                    let identity = (key.clone(), version);
                    if loaded.as_ref() == Some(&identity) {
                        continue;
                    }
                    // 同一身份在退避窗口内已失败过:保留旧图(若有),
                    // 不每秒重试;退避到期或文件变化(identity 变)再试。
                    if let Some((failed_identity, at)) = &failed
                        && failed_identity == &identity
                        && at.elapsed() < RETRY_BACKOFF
                    {
                        continue;
                    }
                    // 基础像素不在缓存(或文件已变):读盘 + 解码一次。
                    let (w, h, rgba) = if let Some((bp, bv, w, h, rgba)) = &base
                        && bp == &path
                        && bv == &version
                    {
                        (*w, *h, rgba.clone())
                    } else {
                        match decode_base(&path) {
                            Ok(decoded) => {
                                base = Some((
                                    path.clone(),
                                    version,
                                    decoded.0,
                                    decoded.1,
                                    decoded.2.clone(),
                                ));
                                decoded
                            }
                            Err(e) => {
                                let mut slot = error_out.lock().unwrap();
                                if slot.as_deref() != Some(e.as_str()) {
                                    log::warn!("背景图片加载失败，稍后重试: {e}");
                                    *slot = Some(e);
                                }
                                loaded = None;
                                failed = Some((identity, std::time::Instant::now()));
                                continue;
                            }
                        }
                    };
                    let image =
                        apply_pixels(w, h, &rgba, f32::from_bits(key.1), f32::from_bits(key.2));
                    *output.lock().unwrap() = Some((key.clone(), image));
                    loaded = Some(identity);
                    failed = None;
                    *error_out.lock().unwrap() = None;
                }
            })
            .expect("wallpaper worker");
        Loader { tx, cache, error }
    })
}
pub(super) fn last_error() -> Option<String> {
    LOADER
        .get()
        .and_then(|loader| loader.error.lock().ok())
        .and_then(|slot| slot.clone())
}
pub(super) fn request(raw: &str, opacity: f32, brightness: f32) -> Option<RenderImage> {
    let loader = loader();
    let key = (raw.to_owned(), opacity.to_bits(), brightness.to_bits());
    let _ = loader.tx.try_send(key.clone());
    loader.cache.try_lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .filter(|(k, _)| k == &key)
            .map(|(_, image)| image.clone())
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_retries_and_reuses_cached_pixels() {
        let dir = std::env::temp_dir().join(format!("tay-wallpaper-worker-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("image.png");
        let _ = std::fs::remove_file(&file);
        let raw = file.to_str().unwrap();
        assert!(request(raw, 1.0, 1.0).is_none());
        let wait = |width| {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(image) = request(raw, 1.0, 1.0)
                    && image.w == width
                {
                    break image;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "worker did not recover"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&file)
            .unwrap();
        let first = wait(2);
        let reused = request(raw, 1.0, 1.0).unwrap();
        assert!(Arc::ptr_eq(&first.rgba, &reused.rgba));
        image::RgbaImage::from_pixel(3, 2, image::Rgba([0, 255, 0, 255]))
            .save(&file)
            .unwrap();
        let replaced = wait(3);
        assert!(!Arc::ptr_eq(&first.rgba, &replaced.rgba));
        // An atomic replacement may preserve both length and mtime (cp -p/rsync).
        // It must still invalidate the cache, including when dimensions are unchanged.
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        let replacement = dir.join("replacement.png");
        std::fs::copy(&file, &replacement).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&file).unwrap().len(),
            std::fs::metadata(&replacement).unwrap().len()
        );
        std::fs::rename(&replacement, &file).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if request(raw, 1.0, 1.0).is_some_and(|image| !Arc::ptr_eq(&image.rgba, &replaced.rgba))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "preserved-mtime replacement was not detected"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn missing_then_created_then_replaced_image_recovers() {
        let dir = std::env::temp_dir().join(format!("tay-wallpaper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("image.png");
        let _ = std::fs::remove_file(&file);
        assert!(decode_base(&file).is_err());
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&file)
            .unwrap();
        let (w, h, rgba) = decode_base(&file).unwrap();
        let first = apply_pixels(w, h, &rgba, 0.5, 0.5);
        assert_eq!(first.rgba[3], 127);
        assert_eq!(first.rgba[0], 127);
        image::RgbaImage::from_pixel(3, 2, image::Rgba([0, 255, 0, 255]))
            .save(&file)
            .unwrap();
        let (w, h, rgba) = decode_base(&file).unwrap();
        let second = apply_pixels(w, h, &rgba, 1.0, 1.0);
        assert_eq!(second.w, 3);
        assert_eq!(second.rgba[1], 255);
        assert!(decode_base(&dir).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
