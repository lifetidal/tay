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
fn decode(path: &std::path::Path, opacity: f32, brightness: f32) -> Result<RenderImage, String> {
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
    let mut pixels = reader.decode().map_err(|e| e.to_string())?.to_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel[3] = (pixel[3] as f32 * opacity) as u8;
        for channel in &mut pixel.0[..3] {
            *channel = (*channel as f32 * brightness) as u8;
        }
    }
    Ok(RenderImage {
        id: u32::MAX,
        clip: None,
        row: 0,
        col: 0,
        cols: 1,
        rows: 1,
        w: pixels.width(),
        h: pixels.height(),
        z: 0,
        rgba: pixels.into_raw().into(),
    })
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
                    match decode(&path, f32::from_bits(key.1), f32::from_bits(key.2)) {
                        Ok(image) => {
                            *output.lock().unwrap() = Some((key.clone(), image));
                            loaded = Some(identity);
                            *error_out.lock().unwrap() = None;
                        }
                        Err(e) => {
                            let mut slot = error_out.lock().unwrap();
                            if slot.as_deref() != Some(e.as_str()) {
                                log::warn!("背景图片加载失败，稍后重试: {e}");
                                *slot = Some(e);
                            }
                            loaded = None;
                        }
                    }
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
        assert!(decode(&file, 1.0, 1.0).is_err());
        image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]))
            .save(&file)
            .unwrap();
        let first = decode(&file, 0.5, 0.5).unwrap();
        assert_eq!(first.rgba[3], 127);
        assert_eq!(first.rgba[0], 127);
        image::RgbaImage::from_pixel(3, 2, image::Rgba([0, 255, 0, 255]))
            .save(&file)
            .unwrap();
        let second = decode(&file, 1.0, 1.0).unwrap();
        assert_eq!(second.w, 3);
        assert_eq!(second.rgba[1], 255);
        assert!(decode(&dir, 1.0, 1.0).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
