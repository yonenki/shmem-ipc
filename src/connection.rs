use std::sync::Arc;
use std::time::Duration;

use memmap2::MmapMut;

use crate::error::Result;
use crate::ring::{RingReceiver, RingSender};
use crate::wait::SpinThenWait;

struct ConnectionDropGuard {
    mmap: MmapMut,
    name: String,
    is_server: bool,
}

impl Drop for ConnectionDropGuard {
    fn drop(&mut self) {
        use crate::header::{ChannelState, GlobalHeader, RingHeader, RingOffsets};
        use crate::platform;
        use std::sync::atomic::Ordering;

        let base = self.mmap.as_ptr();
        let gh = unsafe { GlobalHeader::from_ptr(base) };
        gh.state
            .store(ChannelState::Closed as u32, Ordering::Release);

        let ring_data_size = gh.ring_data_size as usize;
        let offsets = RingOffsets::new(ring_data_size);
        let base = base as *mut u8;
        for offset in [offsets.ring_a_header, offsets.ring_b_header] {
            let rh = unsafe { &*(base.add(offset) as *const RingHeader) };
            rh.writer.notify.fetch_add(1, Ordering::Release);
            platform::futex_wake(&rh.writer.notify);
            rh.reader.notify.fetch_add(1, Ordering::Release);
            platform::futex_wake(&rh.reader.notify);
        }

        if self.is_server {
            let path = platform::shm_path(&self.name);
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Send side of a shared-memory connection.
pub struct SendHalf {
    sender: RingSender,
    wait: SpinThenWait,
    _shared: Arc<ConnectionDropGuard>,
}

unsafe impl Send for SendHalf {}

impl SendHalf {
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender.send(payload, &self.wait, None)
    }

    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender.send(payload, &self.wait, Some(timeout))
    }
}

/// Receive side of a shared-memory connection.
pub struct RecvHalf {
    receiver: RingReceiver,
    wait: SpinThenWait,
    _shared: Arc<ConnectionDropGuard>,
}

unsafe impl Send for RecvHalf {}

impl RecvHalf {
    pub fn recv(&mut self) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, None)
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, Some(timeout))
    }

    pub fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.receiver.recv_into(buf, &self.wait, None)
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.try_recv()
    }
}

/// Shared-memory connection that can be split into send and receive halves.
pub struct ShmemConnection {
    shared: Arc<ConnectionDropGuard>,
    sender: Option<RingSender>,
    receiver: Option<RingReceiver>,
    wait: SpinThenWait,
}

impl ShmemConnection {
    pub(crate) fn new(
        mmap: MmapMut,
        sender: RingSender,
        receiver: RingReceiver,
        wait: SpinThenWait,
        name: String,
        is_server: bool,
    ) -> Self {
        Self {
            shared: Arc::new(ConnectionDropGuard {
                mmap,
                name,
                is_server,
            }),
            sender: Some(sender),
            receiver: Some(receiver),
            wait,
        }
    }

    /// Split the connection into independent send and receive halves.
    pub fn split(mut self) -> (SendHalf, RecvHalf) {
        let sender = self.sender.take().expect("already split");
        let receiver = self.receiver.take().expect("already split");
        let shared = self.shared.clone();

        let send_half = SendHalf {
            sender,
            wait: self.wait.clone(),
            _shared: shared.clone(),
        };

        let recv_half = RecvHalf {
            receiver,
            wait: self.wait.clone(),
            _shared: shared,
        };

        (send_half, recv_half)
    }

    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender
            .as_mut()
            .expect("already split")
            .send(payload, &self.wait, None)
    }

    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender
            .as_mut()
            .expect("already split")
            .send(payload, &self.wait, Some(timeout))
    }

    pub fn recv(&mut self) -> Result<Vec<u8>> {
        self.receiver
            .as_mut()
            .expect("already split")
            .recv(&self.wait, None)
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.receiver
            .as_mut()
            .expect("already split")
            .recv(&self.wait, Some(timeout))
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.as_mut().expect("already split").try_recv()
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }
}
