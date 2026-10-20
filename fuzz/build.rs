//! 为 parser_bytes 生成可 `include!` 的 ptyloop 源码副本。
//!
//! 目标:复用 tayd 的真实协议管线(StreamScanner / CountingHandler 等
//! crate 私有项),而不修改 crates/ 下的任何文件,也不复制出会漂移的
//! 手写副本。
//!
//! `include!` 在宏展开位置不允许内部文档注释(`//!`),所以这里剥离
//! ptyloop.rs 顶部的文档注释块后写到 OUT_DIR。除文档注释外逐字复制。

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR 未设置"),
    );
    let src = manifest_dir.join("../crates/tayd/src/ptyloop.rs");
    println!("cargo:rerun-if-changed={}", src.display());

    let text = std::fs::read_to_string(&src).expect("读取 crates/tayd/src/ptyloop.rs 失败");

    // 只跳过文件开头的空行与 `//!` 文档注释(文件其余部分逐字保留)。
    let mut body = text.as_str();
    loop {
        let Some(pos) = body.find('\n') else {
            // 最后一行没有换行符:若它不是文档注释,保留。
            if !body.trim().is_empty() && !body.trim_start().starts_with("//!") {
                break;
            }
            body = "";
            break;
        };
        let line = &body[..pos];
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//!") {
            body = &body[pos + 1..];
        } else {
            break;
        }
    }

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR 未设置"));
    std::fs::write(out_dir.join("ptyloop_fuzz.rs"), body).expect("写入 OUT_DIR/ptyloop_fuzz.rs 失败");
}
