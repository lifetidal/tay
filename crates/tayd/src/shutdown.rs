//! Signal handlers only set an atomic and write a nonblocking wake byte.
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(crate) struct Termination {
    requested: Arc<AtomicBool>,
    registrations: Vec<signal_hook::SigId>,
}
impl Termination {
    pub(crate) fn install(wake: &UnixStream) -> io::Result<Self> {
        let mut termination = Self {
            requested: Arc::new(AtomicBool::new(false)),
            registrations: Vec::new(),
        };
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
            termination.registrations.push(signal_hook::flag::register(
                signal,
                termination.requested.clone(),
            )?);
            termination
                .registrations
                .push(signal_hook::low_level::pipe::register(
                    signal,
                    wake.try_clone()?,
                )?);
        }
        Ok(termination)
    }
    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }
}
impl Drop for Termination {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn signal_sets_request_and_wakes_without_a_worker_thread() {
        // Isolate process-wide signal handlers from parallel test cases.
        const CHILD: &str = "TAY_SIGNAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shutdown::tests::signal_sets_request_and_wakes_without_a_worker_thread",
                ])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let termination = Termination::install(&writer).unwrap();
        unsafe {
            libc::raise(libc::SIGTERM);
        }
        assert!(termination.requested());
        assert_eq!(reader.read(&mut [0; 8]).unwrap(), 1);
    }
}
