use std::sync::Arc;
use std::time::Duration;

use crate::error::Result;
#[cfg(feature = "tokio")]
use crate::ring::WaitTarget;
use crate::ring::{RingReceiver, RingSender};
use crate::shared::SharedChannel;
use crate::wait::SpinThenWait;

/// Send side of a shared-memory connection.
pub struct SendHalf {
    sender: RingSender,
    wait: SpinThenWait,
    shared: Arc<SharedChannel>,
}

impl SendHalf {
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender.send(payload, &self.wait, None)
    }

    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender.send(payload, &self.wait, Some(timeout))
    }

    /// Closes both directions, including the other split half.
    pub fn close(&self) -> Result<()> {
        self.shared.close()
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
    shared: Arc<SharedChannel>,
}

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

    pub fn close(&self) -> Result<()> {
        self.shared.close()
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
    shared: Arc<SharedChannel>,
    sender: RingSender,
    receiver: RingReceiver,
    wait: SpinThenWait,
}

impl ShmemConnection {
    pub(crate) fn new(
        shared: Arc<SharedChannel>,
        sender: RingSender,
        receiver: RingReceiver,
        wait: SpinThenWait,
    ) -> Self {
        Self {
            shared,
            sender,
            receiver,
            wait,
        }
    }

    pub fn split(self) -> (SendHalf, RecvHalf) {
        (
            SendHalf {
                sender: self.sender,
                wait: self.wait.clone(),
                shared: Arc::clone(&self.shared),
            },
            RecvHalf {
                receiver: self.receiver,
                wait: self.wait,
                shared: self.shared,
            },
        )
    }

    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender.send(payload, &self.wait, None)
    }

    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender.send(payload, &self.wait, Some(timeout))
    }

    pub fn recv(&mut self) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, None)
    }

    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, Some(timeout))
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.try_recv()
    }

    pub fn close(&self) -> Result<()> {
        self.shared.close()
    }

    pub fn name(&self) -> &str {
        &self.shared.name
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn sender_mut(&mut self) -> &mut RingSender {
        &mut self.sender
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn receiver_mut(&mut self) -> &mut RingReceiver {
        &mut self.receiver
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn send_wait_target(&self) -> WaitTarget {
        self.sender.wait_target()
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
