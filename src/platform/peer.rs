use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use crate::error::Error;
use crate::header::RingHeader;

use super::{ChannelWaitSet, PeerCancellation, PeerProcess, wake_handle};

/// Local to one connection, never stored in the shared header. A native wait
/// failure is not evidence of peer death and retains its I/O error identity.
#[derive(Default)]
pub(crate) struct PeerState {
    terminal: OnceLock<io::Result<()>>,
}

impl PeerState {
    pub(crate) fn error(&self) -> Option<Error> {
        self.terminal.get().map(|result| match result {
            Ok(()) => Error::PeerDisconnected,
            Err(err) => Error::Io(match err.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(err.kind(), err.to_string()),
            }),
        })
    }
}

pub(crate) enum PeerWait {
    Exited,
    Cancelled,
}

/// The guard owns the mapping and joins the monitor before unmapping. This
/// context deliberately does not own the guard (nor either connection half).
pub(crate) struct PeerWake {
    state: Arc<PeerState>,
    ring_a: *const RingHeader,
    ring_b: *const RingHeader,
    waits: ChannelWaitSet,
}

// Only atomic notify words are accessed on the monitor thread. The connection
// guard must outlive the thread; PeerMonitor::drop enforces this synchronously.
unsafe impl Send for PeerWake {}

impl PeerWake {
    pub(crate) unsafe fn new(
        state: Arc<PeerState>,
        ring_a: *const RingHeader,
        ring_b: *const RingHeader,
        waits: ChannelWaitSet,
    ) -> Self {
        Self {
            state,
            ring_a,
            ring_b,
            waits,
        }
    }

    fn finish(self, result: io::Result<()>) {
        let _ = self.state.terminal.set(result);
        for (ring, writer, reader) in [
            (
                self.ring_a,
                &self.waits.ring_a_writer,
                &self.waits.ring_a_reader,
            ),
            (
                self.ring_b,
                &self.waits.ring_b_writer,
                &self.waits.ring_b_reader,
            ),
        ] {
            let ring = unsafe { &*ring };
            // Changing the words is essential: the Tokio bridge ignores an OS
            // wake with an unchanged notification sequence.
            ring.writer.notify.fetch_add(1, Ordering::Release);
            wake_handle(writer, &ring.writer.notify);
            ring.reader.notify.fetch_add(1, Ordering::Release);
            wake_handle(reader, &ring.reader.notify);
        }
    }
}

pub(crate) struct PeerMonitor {
    cancellation: Arc<PeerCancellation>,
    thread: Option<JoinHandle<()>>,
}

impl PeerMonitor {
    pub(crate) fn start(process: PeerProcess, wake: PeerWake) -> crate::error::Result<Self> {
        let cancellation = Arc::new(PeerCancellation::new()?);
        let worker_cancellation = cancellation.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("shmem-ipc-peer".to_string())
            .spawn(move || {
                // Validate the native wait on its owning thread before the
                // bootstrap can acknowledge readiness. This is a single
                // waitable-identity probe, never periodic PID polling.
                let ready = process.check_ready();
                let can_wait = ready.is_ok();
                if ready_tx.send(ready).is_err() || !can_wait {
                    return;
                }
                match process.wait(&worker_cancellation) {
                    Ok(PeerWait::Exited) => wake.finish(Ok(())),
                    Ok(PeerWait::Cancelled) => {}
                    Err(err) => wake.finish(Err(err)),
                }
            })?;
        let monitor = Self {
            cancellation,
            thread: Some(thread),
        };
        ready_rx
            .recv()
            .map_err(|err| Error::Io(io::Error::other(err)))??;
        // A process handle/pidfd stays signaled: exit between readiness and the
        // blocking wait is observed without a registration gap.
        Ok(monitor)
    }
}

impl Drop for PeerMonitor {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            // An owned, valid cancellation object cannot normally fail. Do not
            // unwind and detach a thread that might still dereference the mmap.
            if self.cancellation.cancel().is_err() {
                std::process::abort();
            }
            let _ = thread.join();
        }
    }
}
