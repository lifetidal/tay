//! 全局热键:GlobalShortcuts portal(KDE 最成熟后端,见 DESIGN §6.1)。
//!
//! 独立线程跑 async-io 执行器,监听 Activated 信号 → UI 线程。
//! portal 缺失/被拒时静默降级:热键不可用,`tay toggle` 仍可用。

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use futures_lite::StreamExt;

/// 热键触发消息。
#[derive(Clone, Debug)]
pub enum HotkeyTriggered {
    Toggle,
    Status(String),
}

/// 启动热键线程(失败仅告警,不阻塞终端本体)。
/// `tx` 给 UI 线程发触发通知,`wake` 唤醒事件循环(管道写 1 字节)。
pub fn spawn(tx: Sender<HotkeyTriggered>, wake: Arc<UnixStream>, trigger: String) {
    std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            let result = async_io::block_on(async {
                let gs = GlobalShortcuts::new().await?;
                let session = gs.create_session(CreateSessionOptions::default()).await?;
                let shortcut = NewShortcut::new("toggle", "呼出 / 隐藏 Tay 终端")
                    .preferred_trigger(Some(trigger.as_str()));
                let mut activated = gs.receive_activated().await?;
                let binding = gs
                    .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
                    .await?;
                let granted = binding.response()?;
                let description = granted
                    .shortcuts()
                    .iter()
                    .find(|s| s.id() == "toggle")
                    .map(|s| s.trigger_description().to_string())
                    .unwrap_or_else(|| "not assigned; bind tay toggle in desktop settings".into());
                let _ = tx.send(HotkeyTriggered::Status(format!(
                    "Global shortcut: {description}"
                )));
                let _ = (&*wake).write(&[0]);
                log::info!("portal 热键已由系统确认");
                while let Some(a) = activated.next().await {
                    if a.shortcut_id() == "toggle" {
                        let _ = tx.send(HotkeyTriggered::Toggle);
                        let _ = (&*wake).write(&[0]);
                    }
                }
                Ok::<(), ashpd::Error>(())
            });
            if let Err(e) = result {
                log::warn!("全局热键不可用(portal 失败): {e}");
                let _ = tx.send(HotkeyTriggered::Status(format!(
                    "{trigger} unavailable: bind 'tay toggle' in desktop shortcuts"
                )));
                let _ = (&*wake).write(&[0]);
            }
        })
        .ok();
}
