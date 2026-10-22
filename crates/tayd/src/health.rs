//! Read-only daemon snapshot. IPC readers never touch the UI or Wayland objects.
use std::sync::{OnceLock, RwLock};
static STATUS: OnceLock<RwLock<String>> = OnceLock::new();
pub(crate) fn publish(value: String) {
    if let Ok(mut status) = STATUS.get_or_init(Default::default).write() {
        *status = value;
    }
}
pub(crate) fn snapshot() -> String {
    STATUS
        .get_or_init(Default::default)
        .read()
        .map(|s| s.clone())
        .unwrap_or_else(|_| "status unavailable".into())
}
