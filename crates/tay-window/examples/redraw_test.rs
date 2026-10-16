//! 最小复现:RedrawRequested 里再 request_redraw,计数。
use tay_window::{Ctx, Event, Handler};

struct App {
    frames: u32,
}

impl Handler for App {
    fn on_event(&mut self, ctx: &mut Ctx<'_>, event: Event) {
        match event {
            Event::CanCreateSurfaces => {
                eprintln!("[t] surface ready, request redraw");
                ctx.request_redraw();
            },
            Event::RedrawRequested => {
                self.frames += 1;
                eprintln!("[t] frame {}", self.frames);
                ctx.request_redraw();
                if self.frames >= 10 {
                    eprintln!("[t] done");
                    ctx.exit();
                }
            },
            _ => {},
        }
    }
}

fn main() {
    let app = App { frames: 0 };
    tay_window::run("redraw test", (800, 600), app).unwrap();
}
