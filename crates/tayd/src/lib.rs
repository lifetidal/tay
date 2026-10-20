//! Daemon runtime. The executable delegates here; implementation modules stay private.
//! `fuzzing` exposes only bounded byte-stream test entry points.

mod app;
mod blocks;
mod chrome;
mod config;
mod events;
mod hotkey;
mod input;
mod ipc;
mod keys;
mod kitty;
mod persist;
mod ptyloop;
mod quickselect;
mod session;
mod terminal;
mod view;
mod visibility;

mod runtime;
mod shutdown;

/// Start the daemon with process environment and platform services.
pub fn run() {
    runtime::run();
}

mod tabs;

/// Narrow entry points for cargo-fuzz; internal runtime types remain private.
#[cfg(feature = "fuzzing")]
pub mod fuzz_support {
    pub fn parser_bytes(data: &[u8]) {
        crate::ptyloop::exercise_stream(&data[..data.len().min(64 * 1024)]);
    }
    pub fn sixel_dcs(data: &[u8]) {
        crate::ptyloop::exercise_sixel(&data[..data.len().min(64 * 1024)]);
    }
}
