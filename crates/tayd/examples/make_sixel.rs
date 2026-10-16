//! PNG → sixel DCS 序列(测试用):stdout 输出原始字节。
use std::io::Write;

fn main() {
    let path = std::env::args().nth(1).expect("用法: make_sixel <png>");
    let img = image::open(&path).expect("读取 PNG").to_rgba8();
    let (w, h) = img.dimensions();
    let mut sixel = vec![0x1b, b'P', b'q'];
    sixel.extend_from_slice(format!("\"1;1;{w};{h}").as_bytes());
    let data = icy_sixel::SixelImage::from_rgba(img.into_raw(), w as usize, h as usize)
        .encode()
        .expect("编码");
    sixel.extend_from_slice(data.as_bytes());
    sixel.extend_from_slice(b"\x1b\\");
    std::io::stdout().write_all(&sixel).unwrap();
}
