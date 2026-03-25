use std::sync::Arc;
use std::time::Duration;

use memmap2::MmapMut;

use crate::error::Result;
#[cfg(feature = "tokio")]
use crate::ring::WaitTarget;
use crate::ring::{RingReceiver, RingSender};
use crate::wait::SpinThenWait;

struct ConnectionDropGuard {
    mmap: MmapMut,
    wait_set: crate::platform::ChannelWaitSet,
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
        let ring_a = unsafe { &*(base.add(offsets.ring_a_header) as *const RingHeader) };
        ring_a.writer.notify.fetch_add(1, Ordering::Release);
        platform::wake_handle(&self.wait_set.ring_a_writer, &ring_a.writer.notify);
        ring_a.reader.notify.fetch_add(1, Ordering::Release);
        platform::wake_handle(&self.wait_set.ring_a_reader, &ring_a.reader.notify);

        let ring_b = unsafe { &*(base.add(offsets.ring_b_header) as *const RingHeader) };
        ring_b.writer.notify.fetch_add(1, Ordering::Release);
        platform::wake_handle(&self.wait_set.ring_b_writer, &ring_b.writer.notify);
        ring_b.reader.notify.fetch_add(1, Ordering::Release);
        platform::wake_handle(&self.wait_set.ring_b_reader, &ring_b.reader.notify);

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

    #[cfg(feature = "tokio")]
    pub(crate) fn sender_mut(&mut self) -> &mut RingSender {
        &mut self.sender
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn send_wait_target(&self) -> WaitTarget {
        self.sender.wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_spin_count(&self) -> u32 {
        self.wait.spin_count
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

    #[cfg(feature = "tokio")]
    pub(crate) fn receiver_mut(&mut self) -> &mut RingReceiver {
        &mut self.receiver
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn recv_wait_target(&self) -> WaitTarget {
        self.receiver.wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_spin_count(&self) -> u32 {
        self.wait.spin_count
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
        wait_set: crate::platform::ChannelWaitSet,
        wait: SpinThenWait,
        name: String,
        is_server: bool,
    ) -> Self {
        Self {
            shared: Arc::new(ConnectionDropGuard {
                mmap,
                wait_set,
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

    #[cfg(feature = "tokio")]
    pub(crate) fn sender_mut(&mut self) -> &mut RingSender {
        self.sender.as_mut().expect("already split")
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn receiver_mut(&mut self) -> &mut RingReceiver {
        self.receiver.as_mut().expect("already split")
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn send_wait_target(&self) -> WaitTarget {
        self.sender.as_ref().expect("already split").wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn recv_wait_target(&self) -> WaitTarget {
        self.receiver.as_ref().expect("already split").wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_spin_count(&self) -> u32 {
        self.wait.spin_count
    }
}
