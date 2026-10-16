//! kitty graphics 协议解析(DESIGN §7 P1「图像协议三件套」之一)。
//!
//! 序列形如 `ESC _ G <控制键> ; <载荷> ESC \`(APC)。本模块只做纯解析
//! (无 term 依赖,可单测):控制键 → 图像数据(解码 PNG/base64)。
//! 放置位置由 ptyloop 在流中查询光标决定。

use base64::Engine;

/// 解析出的图像传输。
#[derive(Clone, Debug)]
pub struct KittyImage {
    pub id: u32,
    /// 像素尺寸。
    pub w: u32,
    pub h: u32,
    /// 显式声明的单元格尺寸(协议 c/r;缺省由 tayd 按像素推算)。
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    /// RGBA 像素(w*h*4)。
    pub rgba: Vec<u8>,
    /// z 坐标(负 = 文本之下)。
    pub z: i32,
    /// 显示后光标下移到图像下方(C=1,默认)。
    pub cursor_after: bool,
    /// a=d:删除图像。
    pub delete: bool,
    /// p=1:先删除同 id 再放置(与 target 组合,预留)。
    #[allow(dead_code)]
    pub place_after: bool,
    /// a=p 时引用已传输图像的 I=id。
    #[allow(dead_code)]
    pub target: Option<u32>,
}

/// APC 载荷(ESC _ 与 ST 之间的字节)。
pub enum Apc {
    Kitty(KittyImage),
    /// 非 kitty 的 APC(原样转发给终端,由终端决定如何处理)。
    Passthrough(Vec<u8>),
    /// 无效的 kitty 序列(丢弃)。
    Invalid,
}

/// 解析一个完整的 APC 载荷。
pub fn parse_apc(content: &[u8]) -> Apc {
    if content.first() != Some(&b'G') {
        return Apc::Passthrough(content.to_vec());
    }
    let semi = content[1..].iter().position(|&b| b == b';');
    let (ctl, payload) = match semi {
        Some(i) => (&content[1..1 + i], &content[2 + i..]),
        // 无载荷(如删除)。
        None => (&content[1..], &[][..]),
    };

    // 控制键:k=v / 单字母 flag。
    let mut a = String::new();
    let mut id: u32 = 0;
    let mut target: Option<u32> = None;
    let mut f = 32u32;
    let mut w = 0u32;
    let mut h = 0u32;
    let mut cols: Option<u32> = None;
    let mut rows: Option<u32> = None;
    let mut z = 0i32;
    let mut cursor_after = true;
    let mut delete = false;
    let mut place_after = false;
    for kv in ctl.split(|&b| b == b',') {
        let (k, v) = match kv.iter().position(|&b| b == b'=') {
            Some(eq) => (&kv[..eq], &kv[eq + 1..]),
            None => (kv, &[][..]),
        };
        let vint = std::str::from_utf8(v).ok().and_then(|s| s.parse::<u32>().ok());
        match k {
            b"a" => a = String::from_utf8_lossy(v).to_string(),
            b"i" => id = vint.unwrap_or(0),
            b"I" => target = vint,
            b"f" => f = vint.unwrap_or(32),
            b"s" => w = vint.unwrap_or(0),
            b"v" => h = vint.unwrap_or(0),
            b"c" => cols = vint,
            b"r" => rows = vint,
            b"z" => z = std::str::from_utf8(v).ok().and_then(|s| s.parse().ok()).unwrap_or(0),
            b"C" => cursor_after = vint.unwrap_or(1) == 1,
            b"p" => place_after = vint.unwrap_or(0) == 1,
            b"q" | b"m" | b"x" | b"y" | b"X" | b"Y" | b"o" | b"O" | b"d" | b"t" | b"T" => {},
            _ => {},
        }
    }

    // 动作分派(a 缺省 = 传输 t)。
    if a.is_empty() {
        a = "t".to_string();
    }
    match a.as_str() {
        // 删除。
        "d" | "D" => {
            delete = true;
            if let Some(t) = target {
                id = t;
            }
            return Apc::Kitty(KittyImage {
                id,
                w,
                h,
                cols,
                rows,
                rgba: Vec::new(),
                z,
                cursor_after,
                delete,
                place_after,
                target,
            });
        },
        // 传输 / 放置(大小写:大写 = 传输并显示)。
        "t" | "T" | "p" | "P" | "c" => {},
        // 查询/帧/动画等:不支持,忽略。
        _ => return Apc::Invalid,
    }
    if a == "p" || a == "P" {
        if let Some(t) = target {
            id = t;
        }
    }

    // 载荷解码。
    let rgba = match f {
        // PNG(base64)。
        100 => {
            let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(payload) else {
                return Apc::Invalid;
            };
            let Ok(img) = image::load_from_memory(&decoded) else {
                return Apc::Invalid;
            };
            let rgba_img = img.to_rgba8();
            let (iw, ih) = rgba_img.dimensions();
            if w == 0 {
                w = iw;
            }
            if h == 0 {
                h = ih;
            }
            rgba_img.into_raw()
        },
        // 原始 RGBA。
        32 => {
            let need = w as usize * h as usize * 4;
            if payload.len() < need {
                return Apc::Invalid;
            }
            payload[..need].to_vec()
        },
        // 原始 RGB → RGBA。
        24 => {
            let need = w as usize * h as usize * 3;
            if payload.len() < need {
                return Apc::Invalid;
            }
            let mut out = Vec::with_capacity(w as usize * h as usize * 4);
            for i in (0..need).step_by(3) {
                out.extend_from_slice(&payload[i..i + 3]);
                out.push(255);
            }
            out
        },
        _ => return Apc::Invalid,
    };
    if w == 0 || h == 0 || rgba.is_empty() {
        return Apc::Invalid;
    }

    Apc::Kitty(KittyImage {
        id,
        w,
        h,
        cols,
        rows,
        rgba,
        z,
        cursor_after,
        delete,
        place_after,
        target,
    })
}

/// iTerm2 inline 图像(OSC 1337;File=inline=1):返回载体图像。
pub fn parse_iterm2(content: &[u8]) -> Option<KittyImage> {
    let s = String::from_utf8_lossy(content);
    let mut parts = s.split(';');
    if parts.next()? != "1337" {
        return None;
    }
    let file_part = parts.next()?;
    if !file_part.starts_with("File=") {
        return None;
    }
    let attrs = &file_part[5..];
    let (args, b64) = attrs.split_once(':')?;
    let mut inline = false;
    let mut w = 0u32;
    let mut h = 0u32;
    for kv in args.split(':') {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) => (k, v),
            None => (kv, ""),
        };
        match k {
            "inline" => inline = v == "1",
            "width" => {
                w = v.strip_suffix("px").and_then(|n| n.parse().ok()).unwrap_or(0);
            },
            "height" => {
                h = v.strip_suffix("px").and_then(|n| n.parse().ok()).unwrap_or(0);
            },
            _ => {},
        }
    }
    if !inline {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory(&decoded).ok()?.to_rgba8();
    let (iw, ih) = img.dimensions();
    if w == 0 {
        w = iw;
    }
    if h == 0 {
        h = ih;
    }
    Some(KittyImage {
        id: fnv1a(&img.as_raw()[..]) as u32,
        w,
        h,
        cols: None,
        rows: None,
        rgba: img.into_raw(),
        z: 0,
        cursor_after: false,
        delete: false,
        place_after: false,
        target: None,
    })
}

/// FNV-1a(图像 id)。
pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn parses_raw_rgba() {
        // s=1 v=1:1 像素红色。
        let ctl = b"Gf=32,s=1,v=1,i=7";
        let mut content = ctl.to_vec();
        content.push(b';');
        content.extend_from_slice(&[255, 0, 0, 255]);
        let Apc::Kitty(img) = parse_apc(&content) else { panic!("不是 kitty") };
        assert_eq!(img.id, 7);
        assert_eq!((img.w, img.h), (1, 1));
        assert_eq!(img.rgba, vec![255, 0, 0, 255]);
        assert!(img.cursor_after);
    }

    #[test]
    fn parses_png_payload() {
        // 1x1 PNG(base64)。
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let mut content = b"Gf=100,s=1,v=1,i=3;".to_vec();
        content.extend_from_slice(png_b64.as_bytes());
        let Apc::Kitty(img) = parse_apc(&content) else { panic!("不是 kitty") };
        assert_eq!(img.id, 3);
        assert_eq!(img.rgba.len(), 4);
    }

    #[test]
    fn parses_delete() {
        let Apc::Kitty(img) = parse_apc(b"Ga=d,i=9") else { panic!() };
        assert!(img.delete);
        assert_eq!(img.id, 9);
    }

    #[test]
    fn passthrough_non_kitty() {
        assert!(matches!(parse_apc(b"otherstuff"), Apc::Passthrough(_)));
    }

    #[test]
    fn invalid_kitty() {
        assert!(matches!(parse_apc(b"Ga=q"), Apc::Invalid));
        assert!(matches!(parse_apc(b"Gf=100,s=1,v=1;###notbase64###"), Apc::Invalid));
    }

    #[test]
    fn iterm2_inline() {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let content = format!("1337;File=inline=1:{}", png_b64);
        let img = parse_iterm2(content.as_bytes()).expect("应解析成功");
        assert_eq!(img.rgba.len(), 4);
        assert!(!img.cursor_after);
    }

    #[test]
    fn iterm2_non_inline_ignored() {
        let content = "1337;File=name=Zm9v:abcd";
        assert!(parse_iterm2(content.as_bytes()).is_none());
    }
}
