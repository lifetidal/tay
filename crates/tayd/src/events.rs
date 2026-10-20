//! Session → UI mailbox. Replaceable state is coalesced; exit has a reserved slot.
use alacritty_terminal::event::Event;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const MAX_EVENTS: usize = 64;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
#[derive(Default)]
struct Mailbox {
    wake: bool,
    bell: bool,
    title: Option<String>,
    exit: Option<Event>,
    queue: VecDeque<Event>,
    bytes: usize,
}
impl Mailbox {
    fn is_empty(&self) -> bool {
        !self.wake
            && !self.bell
            && self.title.is_none()
            && self.exit.is_none()
            && self.queue.is_empty()
    }
    fn push(&mut self, event: Event) -> bool {
        let notify = self.is_empty();
        match event {
            Event::Wakeup => self.wake = true,
            Event::Bell => self.bell = true,
            Event::Title(mut title) => {
                let mut end = title.len().min(4096);
                while !title.is_char_boundary(end) {
                    end -= 1;
                }
                title.truncate(end);
                self.title = Some(title);
            }
            event @ (Event::Exit | Event::ChildExit(_)) => self.exit = Some(event),
            event => {
                let bytes = event_bytes(&event);
                if self.queue.len() >= MAX_EVENTS || bytes > MAX_TEXT_BYTES - self.bytes {
                    log::warn!("session UI mailbox full; auxiliary event rejected");
                    return false;
                }
                self.bytes += bytes;
                self.queue.push_back(event);
            }
        }
        notify
    }
    fn pop(&mut self) -> Option<Event> {
        if let Some(exit) = self.exit.take() {
            return Some(exit);
        }
        if let Some(title) = self.title.take() {
            return Some(Event::Title(title));
        }
        if let Some(event) = self.queue.pop_front() {
            self.bytes -= event_bytes(&event);
            return Some(event);
        }
        if std::mem::take(&mut self.bell) {
            return Some(Event::Bell);
        }
        std::mem::take(&mut self.wake).then_some(Event::Wakeup)
    }
}
fn event_bytes(event: &Event) -> usize {
    match event {
        Event::ClipboardStore(_, text) | Event::PtyWrite(text) | Event::Title(text) => text.len(),
        _ => 0,
    }
}
#[derive(Clone, Default)]
pub(crate) struct Events(Arc<Mutex<Mailbox>>);
impl Events {
    /// True only on empty → nonempty, so the caller writes one wake byte.
    pub(crate) fn push(&self, event: Event) -> bool {
        self.0.lock().unwrap().push(event)
    }
    /// Acknowledge notifications before reading snapshot/reply slots. Producers
    /// racing with that read must observe an empty mailbox and send a fresh wake.
    pub(crate) fn drain(&self) -> Vec<Event> {
        let mut mailbox = self.0.lock().unwrap();
        let mut events = Vec::new();
        while let Some(event) = mailbox.pop() {
            events.push(event);
        }
        events
    }
    #[cfg(test)]
    pub(crate) fn try_recv(&self) -> Result<Event, std::sync::mpsc::TryRecvError> {
        self.0
            .lock()
            .unwrap()
            .pop()
            .ok_or(std::sync::mpsc::TryRecvError::Empty)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acknowledging_before_state_read_does_not_lose_the_next_wake() {
        let events = Events::default();
        assert!(events.push(Event::Wakeup));
        let batch = events.drain(); // UI acknowledges before reading shared state
        assert_eq!(batch.len(), 1);
        assert!(
            events.push(Event::Wakeup),
            "update racing with the state read needs a new wake"
        );
        assert_eq!(events.drain().len(), 1);
    }

    #[test]
    fn slow_consumer_coalesces_state_and_preserves_exit() {
        let events = Events::default();
        assert!(events.push(Event::Wakeup));
        for i in 0..10000 {
            assert!(!events.push(Event::Wakeup));
            events.push(Event::Title(i.to_string()));
        }
        assert!(matches!(events.try_recv(), Ok(Event::Title(s)) if s == "9999"));
        assert!(matches!(events.try_recv(), Ok(Event::Wakeup)));
        assert!(events.try_recv().is_err());
        for _ in 0..100 {
            events.push(Event::ClipboardStore(
                alacritty_terminal::term::ClipboardType::Clipboard,
                "x".repeat(32768),
            ));
        }
        events.push(Event::Exit);
        assert!(matches!(events.try_recv(), Ok(Event::Exit)));
        let mailbox = events.0.lock().unwrap();
        assert!(mailbox.queue.len() <= MAX_EVENTS);
        assert!(mailbox.bytes <= MAX_TEXT_BYTES);
    }
}
