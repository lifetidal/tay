//! 全局热键:GlobalShortcuts portal(KDE 最成熟后端,见 DESIGN §6.1)。
//!
//! 独立线程跑 async-io 执行器,监听 Activated 信号 → UI 线程。
//! portal 缺失/被拒时静默降级:热键不可用,`tay toggle` 仍可用。

use std::sync::mpsc::Sender;

use ashpd::desktop::global_shortcuts::{
    BindShortcutsOptions, GlobalShortcuts, NewShortcut,
};
use ashpd::desktop::CreateSessionOptions;
use futures_lite::StreamExt;

/// 热键触发消息。
#[derive(Clone, Copy, Debug)]
pub struct HotkeyTriggered;

/// 启动热键线程(失败仅告警,不阻塞终端本体)。
/// `tx` 给 UI 线程发触发通知,`wake` 唤醒事件循环。
pub fn spawn(tx: Sender<HotkeyTriggered>, wake: Sender<()>) {
    std::thread::Builder::new().name("hotkey".into())
        .spawn(move || {
            let result = async_io::block_on(async {
                let gs = GlobalShortcuts::new().await?;
                let session = gs.create_session(CreateSessionOptions::default()).await?;
                let shortcut = NewShortcut::new("toggle", "呼出 / 隐藏 Tay 终端")
                    .preferred_trigger(Some("F12"));
                let _bind = gs
                    .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
                    .await?;
                log::info!("portal 热键会话已建立(首次使用请在系统弹窗中确认 F12)");
                let mut activated = gs.receive_activated().await?;
                while let Some(a) = activated.next().await {
                    if a.shortcut_id() == "toggle" {
                        let _ = tx.send(HotkeyTriggered);
                        let _ = wake.send(());
                    }
                }
                Ok::<(), ashpd::Error>(())
            });
            if let Err(e) = result {
                log::warn!("全局热键不可用(portal 失败): {e}");
            }
        })
        .ok();
}
