//! tayd 库目标(与 bin 共享同一模块树)。
//!
//! 供 fuzz 与集成测试链接内部模块(协议解析、DCS/sixel 解码、
//! 折叠投影等);bin 入口在 main.rs,与库声明同一组模块。

pub mod app;
pub mod blocks;
pub mod chrome;
pub mod config;
pub mod hotkey;
pub mod input;
pub mod ipc;
pub mod keys;
pub mod kitty;
pub mod persist;
pub mod ptyloop;
pub mod quickselect;
pub mod session;
pub mod terminal;
pub mod view;
pub mod visibility;
