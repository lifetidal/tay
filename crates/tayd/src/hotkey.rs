//! Global shortcuts use an explicit host identity and stay independent of mapping.
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, mpsc::SyncSender};
use std::time::{Duration, Instant};

use ashpd::desktop::CreateSessionOptions;
use ashpd::desktop::global_shortcuts::{
    Activated, BindShortcutsOptions, Deactivated, GlobalShortcuts, NewShortcut, ShortcutsChanged,
};
use futures_lite::StreamExt;

pub const APP_ID: &str = "org.tay.Tay";

#[derive(Clone, Debug)]
pub enum HotkeyTriggered {
    Toggle,
    Status(String),
}

type Error = Box<dyn std::error::Error + Send + Sync>;

fn notify(tx: &SyncSender<HotkeyTriggered>, wake: &UnixStream, event: HotkeyTriggered) {
    if tx.try_send(event).is_ok() {
        let _ = (&*wake).write(&[0]);
    }
}

#[derive(Default)]
struct ActivationGate {
    pressed: bool,
    pressed_at: Option<Instant>,
}
impl ActivationGate {
    fn activated(&mut self, own: &str, session: &str, id: &str) -> bool {
        self.activated_at(own, session, id, Instant::now())
    }
    fn activated_at(&mut self, own: &str, session: &str, id: &str, now: Instant) -> bool {
        let stale = self
            .pressed_at
            .is_some_and(|at| now.saturating_duration_since(at) >= Duration::from_secs(2));
        if own != session || id != "toggle" || (self.pressed && !stale) {
            return false;
        }
        self.pressed = true;
        self.pressed_at = Some(now);
        true
    }
    fn deactivated(&mut self, own: &str, session: &str, id: &str) {
        if own == session && id == "toggle" {
            self.pressed = false;
            self.pressed_at = None;
        }
    }
}

fn binding_status(description: Option<&str>) -> String {
    match description.map(str::trim).filter(|s| !s.is_empty()) {
        Some(description) => format!("全局快捷键：{description}"),
        None => "全局快捷键未分配；可从托盘打开 Tay，或在桌面绑定 tay toggle".into(),
    }
}

enum PortalEvent {
    Activated(Activated),
    Deactivated(Deactivated),
    Changed(ShortcutsChanged),
    Disconnected,
    Ignored,
}

// Both edges must come from the same D-Bus message queue. Combining typed
// signal streams can reorder buffered presses ahead of their releases.
fn decode_signal(message: zbus::Message) -> Result<PortalEvent, Error> {
    let header = message.header();
    Ok(match header.member().map(|member| member.as_str()) {
        Some("Activated") => PortalEvent::Activated(message.body().deserialize()?),
        Some("Deactivated") => PortalEvent::Deactivated(message.body().deserialize()?),
        Some("ShortcutsChanged") => PortalEvent::Changed(message.body().deserialize()?),
        _ => PortalEvent::Ignored,
    })
}

async fn listen(
    tx: &SyncSender<HotkeyTriggered>,
    wake: &UnixStream,
    trigger: &str,
) -> Result<(), Error> {
    // Register before any portal call: otherwise the terminal that launched Tay
    // may be inferred as the owner, giving it the wrong saved permissions.
    let connection = zbus::Connection::session().await?;
    ashpd::register_host_app_with_connection(connection.clone(), APP_ID.parse()?).await?;
    let owner = zbus::Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.GlobalShortcuts",
    )
    .await?;
    let owners = owner.receive_owner_changed().await?;
    let gs = GlobalShortcuts::with_connection(connection).await?;
    let session = gs.create_session(CreateSessionOptions::default()).await?;
    // ashpd exposes Session through Serialize, keeping its path accessor private.
    let data = zbus::zvariant::to_bytes(
        zbus::zvariant::serialized::Context::new_dbus(zbus::zvariant::LE, 0),
        &session,
    )?;
    let (session_path, _) = data.deserialize::<zbus::zvariant::OwnedObjectPath>()?;
    // Subscribe before binding, including any events buffered during authorization.
    let signals = owner.receive_all_signals().await?;
    let closed = session.receive_closed().await?;
    let shortcut =
        NewShortcut::new("toggle", "呼出 / 隐藏 Tay 终端").preferred_trigger(Some(trigger));
    let response = gs
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?;
    let granted = response.response()?;
    let description = granted
        .shortcuts()
        .iter()
        .find(|s| s.id() == "toggle")
        .map(|s| s.trigger_description());
    let status = binding_status(description);
    log::info!("{APP_ID}: {status}; session={session_path}");
    notify(tx, wake, HotkeyTriggered::Status(status));
    let events = futures_lite::stream::or(
        signals.map(decode_signal),
        futures_lite::stream::or(
            closed.map(|_| Ok(PortalEvent::Disconnected)),
            owners.map(|_| Ok(PortalEvent::Disconnected)),
        ),
    );
    futures_lite::pin!(events);
    let mut gate = ActivationGate::default();
    while let Some(event) = events.next().await {
        match event? {
            PortalEvent::Activated(a)
                if gate.activated(
                    session_path.as_str(),
                    a.session_handle().as_str(),
                    a.shortcut_id(),
                ) =>
            {
                log::info!("全局快捷键触发：toggle");
                notify(tx, wake, HotkeyTriggered::Toggle);
            }
            PortalEvent::Deactivated(a) => gate.deactivated(
                session_path.as_str(),
                a.session_handle().as_str(),
                a.shortcut_id(),
            ),
            PortalEvent::Changed(c) if c.session_handle().as_str() == session_path.as_str() => {
                let description = c
                    .shortcuts()
                    .iter()
                    .find(|s| s.id() == "toggle")
                    .map(|s| s.trigger_description());
                notify(
                    tx,
                    wake,
                    HotkeyTriggered::Status(binding_status(description)),
                );
            }
            PortalEvent::Disconnected => break,
            _ => {}
        }
    }
    let _ = session.close().await;
    Err("portal 连接或快捷键会话已关闭".into())
}

pub fn spawn(tx: SyncSender<HotkeyTriggered>, wake: Arc<UnixStream>, trigger: String) {
    if let Err(e) = std::thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || {
            loop {
                if let Err(e) = async_io::block_on(listen(&tx, &wake, &trigger)) {
                    log::warn!("全局快捷键不可用：{e}");
                    notify(
                        &tx,
                        &wake,
                        HotkeyTriggered::Status(format!(
                            "{trigger} 未就绪；可从托盘呼出或绑定 tay toggle"
                        )),
                    );
                    // Do not repeatedly show authorization dialogs after refusal.
                    if matches!(
                        e.downcast_ref::<ashpd::Error>(),
                        Some(
                            ashpd::Error::Response(_)
                                | ashpd::Error::Portal(
                                    ashpd::PortalError::NotAllowed(_)
                                        | ashpd::PortalError::Cancelled(_)
                                )
                        )
                    ) {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_secs(10));
            }
        })
    {
        log::error!("无法创建全局快捷键线程：{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_release_does_not_disable_shortcut_forever() {
        let mut gate = ActivationGate::default();
        let now = Instant::now();
        assert!(gate.activated_at("/ours", "/ours", "toggle", now));
        assert!(!gate.activated_at("/ours", "/ours", "toggle", now + Duration::from_millis(100)));
        assert!(gate.activated_at("/ours", "/ours", "toggle", now + Duration::from_secs(3)));
        gate.deactivated("/ours", "/ours", "toggle");
        assert!(gate.activated_at(
            "/ours",
            "/ours",
            "toggle",
            now + Duration::from_millis(3100)
        ));
    }

    #[test]
    fn one_toggle_per_press_and_only_for_own_session() {
        let mut gate = ActivationGate::default();
        assert!(!gate.activated("/ours", "/other", "toggle"));
        assert!(!gate.activated("/ours", "/ours", "other"));
        assert!(gate.activated("/ours", "/ours", "toggle"));
        assert!(!gate.activated("/ours", "/ours", "toggle"));
        gate.deactivated("/ours", "/other", "toggle");
        assert!(!gate.activated("/ours", "/ours", "toggle"));
        gate.deactivated("/ours", "/ours", "toggle");
        assert!(gate.activated("/ours", "/ours", "toggle"));
    }
    #[test]
    fn buffered_signal_queue_preserves_complete_key_presses() {
        let own: zbus::zvariant::OwnedObjectPath = "/ours".try_into().unwrap();
        let mut messages = Vec::new();
        for timestamp in 0..20u64 {
            for member in ["Activated", "Deactivated"] {
                messages.push(
                    zbus::Message::signal(
                        "/org/freedesktop/portal/desktop",
                        "org.freedesktop.portal.GlobalShortcuts",
                        member,
                    )
                    .unwrap()
                    .build(&(
                        own.clone(),
                        "toggle",
                        timestamp,
                        std::collections::HashMap::<String, zbus::zvariant::OwnedValue>::new(),
                    ))
                    .unwrap(),
                );
            }
        }
        let mut gate = ActivationGate::default();
        let count = async_io::block_on(async {
            let mut stream = futures_lite::stream::iter(messages).map(decode_signal);
            let mut count = 0;
            while let Some(event) = stream.next().await {
                match event.unwrap() {
                    PortalEvent::Activated(a) => {
                        count += usize::from(gate.activated(
                            own.as_str(),
                            a.session_handle().as_str(),
                            a.shortcut_id(),
                        ))
                    }
                    PortalEvent::Deactivated(a) => {
                        gate.deactivated(own.as_str(), a.session_handle().as_str(), a.shortcut_id())
                    }
                    _ => panic!("unexpected signal"),
                }
            }
            count
        });
        assert_eq!(count, 20);
        assert!(!gate.pressed);
    }

    #[test]
    fn empty_binding_is_never_reported_as_success() {
        assert!(binding_status(None).contains("未分配"));
        assert!(binding_status(Some(" ")).contains("未分配"));
        assert_eq!(binding_status(Some("Ctrl+`")), "全局快捷键：Ctrl+`");
    }
}
