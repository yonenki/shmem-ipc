use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use memmap2::MmapMut;

use crate::channel::Role;
use crate::error::{Error, Result};
use crate::header::{ChannelState, GlobalHeader, RingHeader, RingOffsets};
use crate::platform::{self, ProcessIdentity, ProcessMonitor};

/// Every blocking/async wait revisits the same channel authority within this interval.
pub(crate) const LIVENESS_QUANTUM: Duration = Duration::from_millis(250);
const PROBE_INTERVAL_MS: u64 = 100;

/// Rings and async wait targets each retain this owner. No raw mmap pointer can
/// outlive it, including a Unix waiter thread or a cancelled Tokio future.
pub(crate) struct SharedChannel {
    pub(crate) name: String,
    pub(crate) role: Role,
    pub(crate) wait_set: platform::ChannelWaitSet,
    peer: Mutex<Option<ProcessMonitor>>,
    started: Instant,
    connect_timeout: Duration,
    next_probe_ms: AtomicU64,
    // Last: native observers/wait objects are released before the mapping.
    mmap: MmapMut,
}

impl SharedChannel {
    pub(crate) fn new(
        mmap: MmapMut,
        wait_set: platform::ChannelWaitSet,
        name: String,
        role: Role,
        connect_timeout: Duration,
        peer: Option<ProcessMonitor>,
    ) -> Self {
        Self {
            name,
            role,
            wait_set,
            peer: Mutex::new(peer),
            started: Instant::now(),
            connect_timeout,
            next_probe_ms: AtomicU64::new(0),
            mmap,
        }
    }

    pub(crate) fn base(&self) -> *mut u8 {
        self.mmap.as_ptr() as *mut u8
    }

    pub(crate) fn header(&self) -> &GlobalHeader {
        unsafe { GlobalHeader::from_ptr(self.mmap.as_ptr()) }
    }

    pub(crate) fn state(&self) -> ChannelState {
        ChannelState::from_u32(self.header().state.load(Ordering::Acquire))
            .unwrap_or(ChannelState::Closed)
    }

    fn terminal(&self) -> Result<()> {
        match ChannelState::from_u32(self.header().state.load(Ordering::Acquire)) {
            Some(ChannelState::PeerDisconnected) => Err(Error::PeerDisconnected),
            Some(ChannelState::Closed) => Err(Error::ChannelClosed),
            Some(ChannelState::Init) | None => Err(Error::InvalidHeader),
            _ => Ok(()),
        }
    }

    /// False means the client has not published its complete identity yet.
    pub(crate) fn check_ready(&self) -> Result<bool> {
        loop {
            let state = self.header().state.load(Ordering::Acquire);
            match ChannelState::from_u32(state) {
                Some(ChannelState::Connected) => break,
                Some(ChannelState::PeerDisconnected) => return Err(Error::PeerDisconnected),
                Some(ChannelState::Closed) => return Err(Error::ChannelClosed),
                Some(ChannelState::Init) | None => return Err(Error::InvalidHeader),
                _ => {}
            }
            if self.started.elapsed() < self.connect_timeout {
                return Ok(false);
            }
            // Retire only the unready state we observed. A concurrently
            // published Connected (or terminal cause) must not be overwritten.
            if self
                .header()
                .state
                .compare_exchange(
                    state,
                    ChannelState::Closed as u32,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.wake_all();
                return Err(Error::TimedOut);
            }
        }

        let elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        if elapsed_ms >= self.next_probe_ms.load(Ordering::Acquire) {
            let mut peer = self
                .peer
                .lock()
                .map_err(|_| Error::Io(std::io::Error::other("process observer lock poisoned")))?;
            // The other split half may already have performed this observation.
            if elapsed_ms >= self.next_probe_ms.load(Ordering::Acquire) {
                self.terminal()?;
                if peer.is_none() {
                    let header = self.header();
                    // Connected was acquired before reading client identity. The
                    // claimant publishes both words before its release transition.
                    let identity = match self.role {
                        Role::Server => ProcessIdentity {
                            pid: header.client_pid.load(Ordering::Relaxed),
                            birth: header.client_birth.load(Ordering::Relaxed),
                        },
                        Role::Client => ProcessIdentity {
                            pid: header.server_pid,
                            birth: header.server_birth,
                        },
                    };
                    match ProcessMonitor::open(identity) {
                        Ok(monitor) => *peer = Some(monitor),
                        Err(Error::PeerDisconnected) => {
                            self.finish(ChannelState::PeerDisconnected);
                            return self.terminal().map(|()| true);
                        }
                        Err(error) => return Err(error),
                    }
                }
                if !peer.as_ref().expect("peer observer installed").is_alive()? {
                    self.finish(ChannelState::PeerDisconnected);
                }
                self.next_probe_ms.store(
                    elapsed_ms.saturating_add(PROBE_INTERVAL_MS),
                    Ordering::Release,
                );
            }
        }
        self.terminal()?;
        Ok(true)
    }

    /// Normal close is idempotent, but never overwrites an observed peer death.
    pub(crate) fn close(&self) -> Result<()> {
        self.finish(ChannelState::Closed);
        match self.terminal() {
            Err(Error::ChannelClosed) => Ok(()),
            other => other,
        }
    }

    fn finish(&self, terminal: ChannelState) {
        let state = &self.header().state;
        let mut current = state.load(Ordering::Acquire);
        loop {
            match ChannelState::from_u32(current) {
                Some(ChannelState::Closed | ChannelState::PeerDisconnected) | None => return,
                _ => {}
            }
            match state.compare_exchange_weak(
                current,
                terminal as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.wake_all();
                    return;
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub(crate) fn wake_all(&self) {
        let offsets = RingOffsets::new(self.header().ring_data_size as usize);
        for (offset, writer, reader) in [
            (
                offsets.ring_a_header,
                &self.wait_set.ring_a_writer,
                &self.wait_set.ring_a_reader,
            ),
            (
                offsets.ring_b_header,
                &self.wait_set.ring_b_writer,
                &self.wait_set.ring_b_reader,
            ),
        ] {
            let ring = unsafe { &*(self.base().add(offset) as *const RingHeader) };
            ring.writer.notify.fetch_add(1, Ordering::Release);
            platform::wake_handle(writer, &ring.writer.notify);
            ring.reader.notify.fetch_add(1, Ordering::Release);
            platform::wake_handle(reader, &ring.reader.notify);
        }
    }
}

impl Drop for SharedChannel {
    fn drop(&mut self) {
        self.finish(ChannelState::Closed);
        if self.role == Role::Server {
            // Listener connection names contain the process birth and listener
            // incarnation; a retired half can only unlink its own generation.
            let _ = std::fs::remove_file(platform::shm_path(&self.name));
        }
    }
}
