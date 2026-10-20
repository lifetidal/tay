#![no_main]

//! DCS(sixel)解码路径。
//!
//! 复刻 ptyloop.rs `StreamScanner::finish_dcs` 的调用点:
//! 扫描器把 `ESC P ... ESC \` 之间的内容存入 `self.dcs`,随后重建完整
//! 序列并调用 `icy_sixel::SixelImage::decode(&seq)`:
//!
//! ```ignore
//! let mut seq = b"\x1bP".to_vec();
//! seq.extend_from_slice(&self.dcs);
//! seq.extend_from_slice(b"\x1b\\");
//! if let Ok(img) = icy_sixel::SixelImage::decode(&seq) { ... }
//! ```
//!
//! 因此 fuzz 输入 = DCS 载荷(ESC P 与 ESC \ 之间的任意字节),
//! 这里按同一方式包装后交给同一解码函数。解码失败(非 sixel DCS,
//! 如 DECRQSS)是正常路径 —— `finish_dcs` 会原样转发,不 panic。
//!
//! 不包 catch_unwind:与生产调用一致,panic 即真实缺陷。

use libfuzzer_sys::fuzz_target;

/// DCS 载荷上限 64 KiB(与 parser_bytes 的输入上限一致)。
const MAX_INPUT: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    let data = &data[..data.len().min(MAX_INPUT)];

    let mut seq = Vec::with_capacity(data.len() + 4);
    seq.extend_from_slice(b"\x1bP");
    seq.extend_from_slice(data);
    seq.extend_from_slice(b"\x1b\\");

    // 与 StreamScanner::finish_dcs 相同的解码调用。
    let _ = icy_sixel::SixelImage::decode(&seq);
});
