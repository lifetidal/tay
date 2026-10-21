//! StatusNotifierItem remains available while the Wayland surface is unmapped.
use crate::ipc::IpcCommand;
use ksni::blocking::TrayMethods;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc,
    mpsc::{self, SyncSender},
};

struct Tray {
    tx: SyncSender<IpcCommand>,
    wake: Arc<UnixStream>,
    shortcut_status: String,
    icon: Vec<ksni::Icon>,
}
impl Tray {
    fn send(&self, action: IpcCommand) {
        if self.tx.try_send(action).is_ok() {
            let _ = (&*self.wake).write(&[0]);
        }
    }
}
impl ksni::Tray for Tray {
    fn id(&self) -> String {
        crate::hotkey::APP_ID.into()
    }
    fn title(&self) -> String {
        "Tay".into()
    }
    fn icon_name(&self) -> String {
        "tay-owl-rounded-v1".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.send(IpcCommand::Toggle);
    }
    fn secondary_activate(&mut self, _: i32, _: i32) {
        self.send(IpcCommand::Settings);
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Tay 终端".into(),
            description: self.shortcut_status.clone(),
            ..Default::default()
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let item = |label: &str, action| {
            StandardItem {
                label: label.into(),
                activate: Box::new(move |tray: &mut Self| tray.send(action)),
                ..Default::default()
            }
            .into()
        };
        vec![
            StandardItem {
                label: self.shortcut_status.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            item("显示 Tay", IpcCommand::Show),
            item("隐藏 Tay", IpcCommand::Hide),
            item("设置", IpcCommand::Settings),
            ksni::MenuItem::Separator,
            item("退出 Tay（结束所有会话）", IpcCommand::Quit),
        ]
    }
}

fn icon() -> Vec<ksni::Icon> {
    let Ok(image) = image::load_from_memory(include_bytes!("../../../packaging/icons/tay.png"))
    else {
        return vec![];
    };
    let image = image
        .resize_exact(64, 64, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let mut data = Vec::with_capacity(64 * 64 * 4);
    for pixel in image.pixels() {
        data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
    }
    vec![ksni::Icon {
        width: 64,
        height: 64,
        data,
    }]
}

pub fn spawn(
    tx: SyncSender<IpcCommand>,
    wake: Arc<UnixStream>,
    status: String,
) -> SyncSender<String> {
    let (updates, rx) = mpsc::sync_channel::<String>(8);
    if let Err(error) = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let tray = Tray {
                tx,
                wake,
                shortcut_status: status,
                icon: icon(),
            };
            // The graphical-session service can start before Plasma's watcher.
            // Keep the item alive and let ksni register when the watcher appears.
            match tray.assume_sni_available(true).spawn() {
                Ok(handle) => {
                    log::info!("Tay 系统托盘已启动");
                    while let Ok(mut status) = rx.recv() {
                        while let Ok(newer) = rx.try_recv() {
                            status = newer;
                        }
                        handle.update(|tray| tray.shortcut_status = status);
                    }
                    handle.shutdown().wait();
                }
                Err(error) => log::warn!("无法启动系统托盘（仍可使用 tay show）：{error}"),
            }
        })
    {
        log::error!("无法创建托盘线程：{error}");
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray as _;
    #[test]
    fn tray_actions_use_the_existing_command_queue_and_wake_loop() {
        let (writer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let (tx, rx) = mpsc::sync_channel(4);
        let mut tray = Tray {
            tx,
            wake: Arc::new(writer),
            shortcut_status: "未分配".into(),
            icon: vec![],
        };
        tray.activate(0, 0);
        assert!(matches!(rx.recv().unwrap(), IpcCommand::Toggle));
        use std::io::Read;
        reader.read_exact(&mut [0]).unwrap();
        tray.secondary_activate(0, 0);
        assert!(matches!(rx.recv().unwrap(), IpcCommand::Settings));
        assert!(tray.tool_tip().description.contains("未分配"));
    }
    #[test]
    fn embedded_icon_has_transparent_corners_and_network_argb_pixels() {
        let icons = icon();
        assert_eq!(icons.len(), 1);
        assert_eq!(icons[0].data.len(), 64 * 64 * 4);
        assert_eq!(icons[0].data[0], 0);
        assert!(icons[0].data.chunks_exact(4).any(|pixel| pixel[0] == 255));
    }
}
