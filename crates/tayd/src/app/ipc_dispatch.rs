//! Dispatch CLI and tray commands on the UI thread.

use super::{Axis, Ctx, PendingClose, TayApp};

impl TayApp {
    /// Drain queued commands. Returns true when a force quit ended the app.
    pub(super) fn dispatch_ipc_commands(&mut self, ctx: &mut Ctx<'_>) -> bool {
        while let Ok(request) = self.ipc_rx.try_recv() {
            request.mark_started();
            let command = request.command;
            use crate::ipc::IpcCommand as Command;
            match command {
                Command::Toggle => self.toggle(ctx),
                Command::Show => self.show(ctx),
                Command::Hide => self.hide(ctx),
                Command::ForceQuit => {
                    request.complete();
                    self.shutdown(ctx);
                    return true;
                }
                Command::ConfirmClose => {
                    if let Some(target) = self.pending_close.take() {
                        self.execute_close(target, ctx);
                    }
                }
                Command::CancelClose => {
                    self.pending_close = None;
                    ctx.request_redraw();
                }
                Command::Quit => {
                    log::info!("收到 quit");
                    self.request_close(PendingClose::Window, ctx);
                }
                Command::SplitRight => self.split_pane(Axis::Columns, ctx),
                Command::SplitDown => self.split_pane(Axis::Rows, ctx),
                Command::NextPane => self.next_pane(ctx),
                Command::ClosePane => {
                    self.request_close(PendingClose::Panes(vec![self.active_pane]), ctx)
                }
                Command::ZoomPane => self.zoom_pane(ctx),
                Command::NewTab => {
                    self.new_tab(ctx);
                    self.show(ctx);
                }
                Command::NextTab => {
                    self.switch_tab((self.active_index() + 1) % self.layouts.len(), ctx);
                }
                Command::CloseTab => self.close_tab(self.active_index(), ctx),
                Command::Maximize => {
                    if !self.maximized {
                        self.chrome_action(crate::chrome::Action::Maximize, ctx);
                    }
                    self.show(ctx);
                }
                Command::Restore => {
                    if self.maximized {
                        self.chrome_action(crate::chrome::Action::Maximize, ctx);
                    }
                    self.show(ctx);
                }
                Command::Settings => {
                    self.open_settings(ctx, None);
                }
                Command::SettingsPage(page) => {
                    self.open_settings(ctx, Some(page));
                }
                Command::Pin => {
                    let pinned = self.vis.toggle_pin();
                    log::info!(
                        "固定显示: {}",
                        if pinned {
                            "开(失焦不再自动隐藏)"
                        } else {
                            "关"
                        }
                    );
                }
            }
            request.complete();
        }
        false
    }

    fn open_settings(&mut self, ctx: &mut Ctx<'_>, page: Option<u8>) {
        self.animate_settings(true);
        if let Some(page) = page {
            self.settings_page = page;
        }
        self.settings_focus = 0;
        self.editing = None;
        self.settings_drag = None;
        self.settings_feedback.clear();
        self.show(ctx);
        ctx.request_redraw();
    }
}
