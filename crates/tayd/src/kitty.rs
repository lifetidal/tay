//! kitty graphics 协议解析(DESIGN §7 P1「图像协议三件套」之一)。
//!
//! 序列形如 `ESC _ G <控制键> ; <载荷> ESC \`(APC)。本模块只做纯解析
//! (无 term 依赖,可单测):控制键 → 图像数据(解码 PNG/base64)。
//! 放置位置由 ptyloop 在流中查询光标决定。

use base64::Engine;

/// 图像后光标移动方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorAfter {
    /// 不移动光标(kitty C=1)。
    None,
    /// 协议默认:光标右移 cols、再下移 rows(放置矩形之后)。
    Move,
    /// 光标下移 N 行(sixel 语义)。
    Down,
    /// 光标右移 N 列(iTerm2 inline 语义)。
    Right,
}

/// a 键动作(协议语义)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KittyAction {
    /// a=T / 缺省:传输并显示。
    TransmitDisplay,
    /// a=t:仅传输(进缓存,不显示;协议允许后续 a=p 放置)。
    TransmitOnly,
    /// a=p:放置已传输图像(显示缓存中的图)。
    Place,
    /// a=d:删除(不带定位键 = 删除全部)。
    Delete,
}

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
    /// 解码像素(共享句柄:一次解码,放置/渲染共享引用,
    /// 不逐帧复制 —— 架构 §6 资源策略)。
    pub rgba: std::sync::Arc<[u8]>,
    /// z 坐标(负 = 文本之下)。
    pub z: i32,
    /// 显示后光标移动(kitty C=1 / sixel / iTerm2 语义)。
    pub cursor_after: CursorAfter,
    /// a 键动作。
    pub action: KittyAction,
    /// p=1:先删除同 id 再放置(与 target 组合,预留)。
    #[allow(dead_code)]
    pub place_after: bool,
    /// 删除定位 I=number(协议:number 非 id,定位最新同号)。
    #[allow(dead_code)]
    pub target: Option<u32>,
    /// 分块标记(仅流解析期间使用,渲染忽略):None = 无 m 键的普通
    /// 传输,Some(true) = m=1(还有后续块),Some(false) = m=0(末块)。
    /// 分块重组由 ptyloop 的 StreamScanner 完成:首块携带全部控制键,
    /// 后续块仅 m 键有效,载荷按序拼接。
    #[allow(dead_code)]
    pub more: Option<bool>,
}

/// APC 载荷(ESC _ 与 ST 之间的字节)。
pub enum Apc {
    Kitty(KittyImage),
    /// 查询/放置请求:按协议必须回 OK / ENOENT 应答
    /// (`action` = 'q' 查询, 'p' 放置已传输图像)。tayd 没有传输缓存,
    /// q 回 OK(声明支持),p 回 ENOENT(诚实拒绝)。
    Query {
        id: u32,
        action: char,
    },
    /// 非 kitty 的 APC(原样转发给终端,由终端决定如何处理)。
    Passthrough(Vec<u8>),
    /// 无效的 kitty 序列(丢弃)。
    Invalid,
}

/// 解码后像素上限:4096×4096(超出拒绝,防解压炸弹)。
const MAX_PIXELS: u64 = 4096 * 4096;
/// 解码后 RGBA 字节上限:64 MiB。
const MAX_RGBA_BYTES: usize = 64 * 1024 * 1024;

/// 解码 kitty 载荷:`f` 为格式(100=PNG base64,32=RGBA,24=RGB)。
/// **协议规定 24/32 的载荷也是 base64 编码**(整串编码后才允许分块),
/// `w`/`h` 缺省时由数据推算(PNG)。全程 checked arithmetic + 尺寸上限。
pub fn decode_payload(
    f: u32,
    w: &mut u32,
    h: &mut u32,
    payload: &[u8],
) -> Option<std::sync::Arc<[u8]>> {
    // 先解 base64(三种格式皆 base64 on the wire)。
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let rgba = match f {
        // PNG:尺寸从数据读取。
        100 => {
            let img = image::load_from_memory(&decoded).ok()?;
            let rgba_img = img.to_rgba8();
            let (iw, ih) = rgba_img.dimensions();
            let pixels = iw as u64 * ih as u64;
            if pixels == 0 || pixels > MAX_PIXELS {
                return None;
            }
            if *w == 0 {
                *w = iw;
            }
            if *h == 0 {
                *h = ih;
            }
            rgba_img.into_raw().into()
        }
        // 原始 RGBA:尺寸必须由 s/v 给出。
        32 => {
            let need = (*w as u64).checked_mul(*h as u64)?.checked_mul(4)?;
            if need == 0 || need > MAX_RGBA_BYTES as u64 || decoded.len() < need as usize {
                return None;
            }
            decoded[..need as usize].to_vec().into()
        }
        // 原始 RGB → RGBA。
        24 => {
            let need = (*w as u64).checked_mul(*h as u64)?.checked_mul(3)?;
            if need == 0 || need > MAX_RGBA_BYTES as u64 * 3 / 4 || decoded.len() < need as usize {
                return None;
            }
            let mut out = Vec::with_capacity(need as usize / 3 * 4);
            for i in (0..need as usize).step_by(3) {
                out.extend_from_slice(&decoded[i..i + 3]);
                out.push(255);
            }
            out.into()
        }
        _ => return None,
    };
    Some(rgba)
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
    // 协议默认:C 缺省 = 放置后光标右移 cols、下移 rows(Move);
    // C=1 显式禁止移动。
    let mut cursor_after = CursorAfter::Move;
    let mut place_after = false;
    let mut more: Option<bool> = None;
    for kv in ctl.split(|&b| b == b',') {
        let (k, v) = match kv.iter().position(|&b| b == b'=') {
            Some(eq) => (&kv[..eq], &kv[eq + 1..]),
            None => (kv, &[][..]),
        };
        let vint = std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.parse::<u32>().ok());
        match k {
            b"a" => a = String::from_utf8_lossy(v).to_string(),
            b"i" => id = vint.unwrap_or(0),
            b"I" => target = vint,
            b"f" => f = vint.unwrap_or(32),
            b"s" => w = vint.unwrap_or(0),
            b"v" => h = vint.unwrap_or(0),
            b"c" => cols = vint,
            b"r" => rows = vint,
            b"z" => {
                z = std::str::from_utf8(v)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0)
            }
            b"C" => {
                // C=1 = 不移动;其余(含缺省)= 协议默认移动。
                cursor_after = if vint == Some(1) {
                    CursorAfter::None
                } else {
                    CursorAfter::Move
                }
            }
            b"p" => place_after = vint.unwrap_or(0) == 1,
            b"m" => more = Some(vint == Some(1)),
            b"q" | b"x" | b"y" | b"X" | b"Y" | b"o" | b"O" | b"d" | b"t" | b"T" => {}
            _ => {}
        }
    }

    // 动作分派(协议:a 缺省 = T,传输并显示)。
    if a.is_empty() {
        a = "T".to_string();
    }
    match a.as_str() {
        // 删除。协议:不带其它键 = 删除全部可见图像;带 I/i 定位。
        // id 缺省 0 = "全部"语义(与合法 id ≥ 1 不冲突)。
        "d" | "D" => {
            if let Some(t) = target {
                id = t;
            }
            return Apc::Kitty(KittyImage {
                id,
                w,
                h,
                cols,
                rows,
                rgba: std::sync::Arc::from([]),
                z,
                cursor_after,
                action: KittyAction::Delete,
                place_after,
                target,
                more,
            });
        }
        // 查询:必须携带 i(协议:i 为 1..=4294967295,不能为 0)。
        "q" => {
            if id == 0 {
                return Apc::Invalid;
            }
            return Apc::Query { id, action: 'q' };
        }
        // 放置已传输图像:需要 i 定位。载荷/尺寸来自传输缓存,
        // 解析期 w/h/payload 为空,由分块处理器补齐。
        "p" | "P" => {
            if id == 0 {
                return Apc::Invalid;
            }
            return Apc::Kitty(KittyImage {
                id,
                w: 0,
                h: 0,
                cols,
                rows,
                rgba: std::sync::Arc::from([]),
                z,
                cursor_after,
                action: KittyAction::Place,
                place_after,
                target,
                more,
            });
        }
        // 传输:重传同 i 先删旧图再更新。
        "t" | "T" | "c" => {}
        // 帧/动画等:不支持,忽略。
        _ => return Apc::Invalid,
    }
    if (a == "p" || a == "P")
        && let Some(t) = target
    {
        id = t;
    }

    // 载荷解码。分块传输(m 键存在)时块载荷可能是任意切片
    // (如 base64 跨块切分),不能按块解码 —— 原样返回,由扫描器在
    // 末块拼接后统一重组解析。
    let rgba: std::sync::Arc<[u8]> = if more.is_some() {
        payload.to_vec().into()
    } else {
        let Some(rgba) = decode_payload(f, &mut w, &mut h, payload) else {
            return Apc::Invalid;
        };
        rgba
    };
    // 走到这里的动作只有 t/T/c(a=p/d/q 已提前返回),
    // 必须有完整数据(载荷/尺寸)。
    if more.is_none() && (w == 0 || h == 0 || rgba.is_empty()) {
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
        // 走到这里的动作只可能是 t/T/c(其余已提前返回)。
        action: match a.as_str() {
            "t" => KittyAction::TransmitOnly,
            _ => KittyAction::TransmitDisplay,
        },
        place_after,
        target,
        more,
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
                w = v
                    .strip_suffix("px")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
            "height" => {
                h = v
                    .strip_suffix("px")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
            _ => {}
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
        rgba: img.into_raw().into(),
        z: 0,
        cursor_after: CursorAfter::Right,
        action: KittyAction::TransmitDisplay,
        place_after: false,
        more: None,
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
        // s=1 v=1:1 像素红色。协议规定 f=32 载荷 base64 编码。
        let ctl = b"Gf=32,s=1,v=1,i=7";
        let mut content = ctl.to_vec();
        content.push(b';');
        content.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode([255, 0, 0, 255])
                .as_bytes(),
        );
        let Apc::Kitty(img) = parse_apc(&content) else {
            panic!("不是 kitty")
        };
        assert_eq!(img.id, 7);
        assert_eq!((img.w, img.h), (1, 1));
        assert_eq!(&*img.rgba, &[255, 0, 0, 255][..]);
        assert_eq!(img.cursor_after, CursorAfter::Move, "协议默认:光标移动");
    }

    #[test]
    fn parses_png_payload() {
        // 1x1 PNG(base64)。
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let mut content = b"Gf=100,s=1,v=1,i=3;".to_vec();
        content.extend_from_slice(png_b64.as_bytes());
        let Apc::Kitty(img) = parse_apc(&content) else {
            panic!("不是 kitty")
        };
        assert_eq!(img.id, 3);
        assert_eq!(img.rgba.len(), 4);
    }

    /// P3a 协议标准样本:kitty graphics 规范中的典型传输形态
    /// (`\e_Ga=T,f=32,s=2,v=2,i=7;<rgba>\e\`)。任意切点的流等价性
    /// 由 ptyloop 的 chunk 等价测试覆盖。
    #[test]
    fn standard_sample_parses() {
        // 协议标准样本:base64 载荷(不是 raw)。
        let mut ctl = b"Ga=T,f=32,s=2,v=2,i=7;".to_vec();
        ctl.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode([
                    255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
                ])
                .as_bytes(),
        );
        let Apc::Kitty(img) = parse_apc(&ctl) else {
            panic!("标准样本解析失败")
        };
        assert_eq!((img.w, img.h), (2, 2));
        assert_eq!(img.id, 7);
        assert_eq!(img.rgba.len(), 16);
        assert_eq!(img.more, None, "无 m 键 = 独立完整传输");
    }

    /// 协议:查询(a=q)必须带 i,解析为 Query 以便应答。
    #[test]
    fn query_parses_with_id() {
        let Apc::Query { id, action } = parse_apc(b"Ga=q,i=42") else {
            panic!("a=q 应为 Query")
        };
        assert_eq!(id, 42);
        assert_eq!(action, 'q');
        // 无 i 的查询是协议错误(Invalid)。
        assert!(matches!(parse_apc(b"Ga=q"), Apc::Invalid));
    }

    /// 协议:放置(a=p)定位已传输图像;i 缺失无效。
    /// 解析为 Place 动作的 KittyImage(像素/尺寸由传输缓存补齐)。
    #[test]
    fn place_parses_with_id() {
        let Apc::Kitty(img) = parse_apc(b"Ga=p,i=7") else {
            panic!("a=p 应为 Place 动作的 KittyImage")
        };
        assert_eq!(img.id, 7);
        assert_eq!(img.action, KittyAction::Place);
        assert!(matches!(parse_apc(b"Ga=p"), Apc::Invalid));
    }

    /// 协议:C=1 禁止光标移动;C 缺省/其它值 = 默认移动。
    #[test]
    fn cursor_movement_defaults() {
        let Apc::Kitty(img) = parse_apc(b"Ga=T,f=32,s=1,v=1,i=1;/wAA/w==") else {
            panic!("解析失败")
        };
        assert_eq!(img.cursor_after, CursorAfter::Move, "C 缺省 = 移动");
        let Apc::Kitty(img) = parse_apc(b"Ga=T,f=32,s=1,v=1,i=1,C=1;/wAA/w==") else {
            panic!("解析失败")
        };
        assert_eq!(img.cursor_after, CursorAfter::None, "C=1 = 不移动");
    }

    /// 协议:a=d 不带定位键 = 删除全部可见图像(id 0 语义)。
    #[test]
    fn delete_all_has_zero_id() {
        let Apc::Kitty(img) = parse_apc(b"Ga=d") else {
            panic!("a=d 应为 Kitty")
        };
        assert_eq!(img.action, KittyAction::Delete);
        assert_eq!(img.id, 0, "id=0 表示删除全部");
    }

    #[test]
    fn parses_delete() {
        let Apc::Kitty(img) = parse_apc(b"Ga=d,i=9") else {
            panic!()
        };
        assert_eq!(img.action, KittyAction::Delete);
        assert_eq!(img.id, 9);
    }

    #[test]
    fn passthrough_non_kitty() {
        assert!(matches!(parse_apc(b"otherstuff"), Apc::Passthrough(_)));
    }

    #[test]
    fn invalid_kitty() {
        assert!(matches!(parse_apc(b"Ga=q"), Apc::Invalid));
        assert!(matches!(
            parse_apc(b"Gf=100,s=1,v=1;###notbase64###"),
            Apc::Invalid
        ));
    }

    #[test]
    fn iterm2_inline() {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let content = format!("1337;File=inline=1:{}", png_b64);
        let img = parse_iterm2(content.as_bytes()).expect("应解析成功");
        assert_eq!(img.rgba.len(), 4);
        assert_eq!(img.cursor_after, CursorAfter::Right);
    }

    #[test]
    fn iterm2_non_inline_ignored() {
        let content = "1337;File=name=Zm9v:abcd";
        assert!(parse_iterm2(content.as_bytes()).is_none());
    }
}

/// 图像总量预算的逐出决策(纯函数,第八轮资源策略):
/// 解码 RGBA 总量超 `budget` 时逐出**最旧**的放置实例,至少保留
/// 最新一张(单张超预算由解码侧 64 MiB 上限负责,这里不逐出全部)。
/// 返回应逐出的最旧数量。
pub fn eviction_count(sizes: &[usize], budget: usize) -> usize {
    let total: usize = sizes.iter().sum();
    if total <= budget || sizes.is_empty() {
        return 0;
    }
    let mut drop = 0;
    let mut cur = total;
    for s in sizes.iter() {
        if cur <= budget {
            break;
        }
        cur -= s;
        drop += 1;
    }
    if drop >= sizes.len() {
        drop = sizes.len() - 1;
    }
    drop
}

#[cfg(test)]
mod budget_tests {
    use super::eviction_count;

    #[test]
    fn evicts_oldest_until_within_budget() {
        // 总量 300,预算 100:逐出最旧的 100+80+60,保留最后 60。
        assert_eq!(eviction_count(&[100, 80, 60, 60], 100), 3);
        // 已在预算内:不逐出。
        assert_eq!(eviction_count(&[100, 80, 60, 60], 300), 0);
        // 单张超预算:仍保留最新一张(单图上限在解码侧)。
        assert_eq!(eviction_count(&[10, 10, 500], 100), 2);
        // 空集合。
        assert_eq!(eviction_count(&[], 0), 0);
    }
}
