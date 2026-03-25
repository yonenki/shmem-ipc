use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::header::{ChannelState, GlobalHeader, RingHeader};
use crate::platform;
use crate::wait::WaitStrategy;

pub const DEFAULT_RING_DATA_SIZE: usize = 16 * 1024 * 1024;
pub const MSG_HEADER_SIZE: usize = 8;

#[cfg(feature = "tokio")]
#[derive(Clone)]
pub(crate) struct WaitTarget {
    handle: platform::WaitHandle,
    notify: *const AtomicU32,
    parked: *const AtomicU32,
}

#[cfg(feature = "tokio")]
impl WaitTarget {
    pub(crate) fn new(
        handle: platform::WaitHandle,
        notify: *const AtomicU32,
        parked: *const AtomicU32,
    ) -> Self {
        Self {
            handle,
            notify,
            parked,
        }
    }

    pub(crate) fn handle(&self) -> &platform::WaitHandle {
        &self.handle
    }

    pub(crate) fn notify(&self) -> &AtomicU32 {
        unsafe { &*self.notify }
    }

    pub(crate) fn parked(&self) -> &AtomicU32 {
        unsafe { &*self.parked }
    }
}

#[cfg(feature = "tokio")]
unsafe impl Send for WaitTarget {}
#[cfg(feature = "tokio")]
unsafe impl Sync for WaitTarget {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrySendResult {
    Sent,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeekedMessage {
    read_cursor: u64,
    payload_len: usize,
    seq: u32,
}

impl PeekedMessage {
    pub(crate) fn payload_len(self) -> usize {
        self.payload_len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TryPeekResult {
    Empty,
    Ready(PeekedMessage),
}

pub struct RingSender {
    base: *mut u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    writer_wait: platform::WaitHandle,
    reader_wait: platform::WaitHandle,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize,
    next_seq: u32,
    cached_wc: u64,
    cached_rc: u64,
}

unsafe impl Send for RingSender {}

impl RingSender {
    pub unsafe fn new(
        base: *mut u8,
        ring_header: *const RingHeader,
        global_header: *const GlobalHeader,
        writer_wait: platform::WaitHandle,
        reader_wait: platform::WaitHandle,
        data_offset: usize,
        ring_data_size: usize,
    ) -> Self {
        debug_assert!(ring_data_size.is_power_of_two());
        Self {
            base,
            ring_header,
            global_header,
            writer_wait,
            reader_wait,
            data_offset,
            ring_data_size,
            ring_mask: ring_data_size - 1,
            next_seq: 1,
            cached_wc: 0,
            cached_rc: 0,
        }
    }

    pub fn send(
        &mut self,
        payload: &[u8],
        wait: &impl WaitStrategy,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let msg_len = self.message_size(payload.len())?;

        loop {
            match self.try_send_once(payload)? {
                TrySendResult::Sent => return Ok(()),
                TrySendResult::Full => {
                    let wc = self.cached_wc;
                    let ring_data_size = self.ring_data_size;
                    let read_cursor = self.read_cursor();
                    let reader_notify = self.reader_notify();
                    let reader_parked = self.reader_parked();
                    let state = self.state();

                    let new_rc = wait.wait_until(
                        &self.reader_wait,
                        reader_notify,
                        reader_parked,
                        timeout,
                        || {
                            let rc = read_cursor.load(Ordering::Acquire);
                            let free = ring_data_size as u64 - (wc - rc);
                            if free >= msg_len as u64 {
                                Ok(Some(rc))
                            } else {
                                match ChannelState::from_u32(state.load(Ordering::Acquire)) {
                                    Some(channel_state) if channel_state.is_active() => Ok(None),
                                    _ => Err(Error::ChannelClosed),
                                }
                            }
                        },
                    )?;
                    self.cached_rc = new_rc;
                }
            }
        }
    }

    pub(crate) fn try_send_once(&mut self, payload: &[u8]) -> Result<TrySendResult> {
        self.check_channel_active()?;

        let msg_len = self.message_size(payload.len())?;
        let wc = self.cached_wc;

        if !self.has_space(wc, msg_len) {
            let fresh_rc = self.read_cursor().load(Ordering::Acquire);
            self.cached_rc = fresh_rc;
            if !self.has_space(wc, msg_len) {
                return Ok(TrySendResult::Full);
            }
        }

        self.publish_message(wc, payload);
        Ok(TrySendResult::Sent)
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_target(&self) -> WaitTarget {
        WaitTarget::new(
            self.reader_wait.clone(),
            self.reader_notify() as *const AtomicU32,
            self.reader_parked() as *const AtomicU32,
        )
    }

    fn publish_message(&mut self, wc: u64, payload: &[u8]) {
        let len_bytes = (payload.len() as u32).to_le_bytes();
        let seq_bytes = self.next_seq.to_le_bytes();
        self.write_ring(wc, &len_bytes);
        self.write_ring(wc + 4, &seq_bytes);
        self.write_ring(wc + MSG_HEADER_SIZE as u64, payload);

        let new_wc = wc + (MSG_HEADER_SIZE + payload.len()) as u64;
        self.write_cursor().store(new_wc, Ordering::Release);
        self.cached_wc = new_wc;
        self.next_seq = self.next_seq.wrapping_add(1);

        self.writer_notify().fetch_add(1, Ordering::Release);
        if self.writer_parked().load(Ordering::Acquire) != 0 {
            platform::wake_handle(&self.writer_wait, self.writer_notify());
        }
    }

    fn has_space(&self, wc: u64, msg_len: usize) -> bool {
        let free = self.ring_data_size as u64 - (wc - self.cached_rc);
        free >= msg_len as u64
    }

    fn message_size(&self, payload_len: usize) -> Result<usize> {
        let msg_len = MSG_HEADER_SIZE + payload_len;
        if msg_len > self.ring_data_size {
            return Err(Error::MessageTooLarge {
                size: payload_len,
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }
        Ok(msg_len)
    }

    fn write_ring(&self, cursor: u64, data: &[u8]) {
        let start = (cursor as usize) & self.ring_mask;
        let first_len = data.len().min(self.ring_data_size - start);
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.base.add(self.data_offset + start),
                first_len,
            );
            if first_len < data.len() {
                std::ptr::copy_nonoverlapping(
                    data.as_ptr().add(first_len),
                    self.base.add(self.data_offset),
                    data.len() - first_len,
                );
            }
        }
    }

    fn write_cursor(&self) -> &AtomicU64 {
        unsafe { &(*self.ring_header).writer.write_cursor }
    }

    fn read_cursor(&self) -> &AtomicU64 {
        unsafe { &(*self.ring_header).reader.read_cursor }
    }

    fn writer_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.notify }
    }

    fn writer_parked(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.parked }
    }

    fn reader_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.notify }
    }

    fn reader_parked(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.parked }
    }

    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.global_header).state }
    }

    fn check_channel_active(&self) -> Result<()> {
        match ChannelState::from_u32(self.state().load(Ordering::Acquire)) {
            Some(state) if state.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }
}

pub struct RingReceiver {
    base: *const u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    writer_wait: platform::WaitHandle,
    reader_wait: platform::WaitHandle,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize,
    expected_seq: u32,
    cached_rc: u64,
}

unsafe impl Send for RingReceiver {}

impl RingReceiver {
    pub unsafe fn new(
        base: *const u8,
        ring_header: *const RingHeader,
        global_header: *const GlobalHeader,
        writer_wait: platform::WaitHandle,
        reader_wait: platform::WaitHandle,
        data_offset: usize,
        ring_data_size: usize,
    ) -> Self {
        debug_assert!(ring_data_size.is_power_of_two());
        Self {
            base,
            ring_header,
            global_header,
            writer_wait,
            reader_wait,
            data_offset,
            ring_data_size,
            ring_mask: ring_data_size - 1,
            expected_seq: 1,
            cached_rc: 0,
        }
    }

    pub fn recv(&mut self, wait: &impl WaitStrategy, timeout: Option<Duration>) -> Result<Vec<u8>> {
        let message = self.wait_for_message(wait, timeout)?;
        let mut buf = vec![0u8; message.payload_len()];
        self.copy_current_message(message, &mut buf)?;
        self.commit_current_message(message)?;
        Ok(buf)
    }

    pub fn recv_into(
        &mut self,
        buf: &mut [u8],
        wait: &impl WaitStrategy,
        timeout: Option<Duration>,
    ) -> Result<usize> {
        let message = self.wait_for_message(wait, timeout)?;
        let payload_len = message.payload_len();

        if payload_len > buf.len() {
            self.commit_current_message(message)?;
            return Err(Error::MessageTooLarge {
                size: payload_len,
                max: buf.len(),
            });
        }

        self.copy_current_message(message, &mut buf[..payload_len])?;
        self.commit_current_message(message)?;
        Ok(payload_len)
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        match self.try_peek_message()? {
            TryPeekResult::Empty => Ok(None),
            TryPeekResult::Ready(message) => {
                let mut buf = vec![0u8; message.payload_len()];
                self.copy_current_message(message, &mut buf)?;
                self.commit_current_message(message)?;
                Ok(Some(buf))
            }
        }
    }

    pub(crate) fn try_peek_message(&self) -> Result<TryPeekResult> {
        let rc = self.cached_rc;
        let wc = self.write_cursor().load(Ordering::Acquire);

        if wc - rc < MSG_HEADER_SIZE as u64 {
            return Ok(TryPeekResult::Empty);
        }

        let (payload_len, seq) = self.read_msg_header(rc);
        if payload_len > self.ring_data_size - MSG_HEADER_SIZE {
            return Err(Error::MessageTooLarge {
                size: payload_len,
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        let msg_len = MSG_HEADER_SIZE + payload_len;
        if wc - rc < msg_len as u64 {
            return Ok(TryPeekResult::Empty);
        }

        Ok(TryPeekResult::Ready(PeekedMessage {
            read_cursor: rc,
            payload_len,
            seq,
        }))
    }

    pub(crate) fn copy_current_message(
        &self,
        message: PeekedMessage,
        buf: &mut [u8],
    ) -> Result<usize> {
        if message.payload_len() > buf.len() {
            return Err(Error::MessageTooLarge {
                size: message.payload_len(),
                max: buf.len(),
            });
        }

        self.read_ring(
            message.read_cursor + MSG_HEADER_SIZE as u64,
            &mut buf[..message.payload_len()],
        );
        Ok(message.payload_len())
    }

    pub(crate) fn commit_current_message(&mut self, message: PeekedMessage) -> Result<()> {
        self.validate_seq(message.seq)?;

        let new_rc = message.read_cursor + (MSG_HEADER_SIZE + message.payload_len()) as u64;
        self.read_cursor().store(new_rc, Ordering::Release);
        self.cached_rc = new_rc;

        self.reader_notify().fetch_add(1, Ordering::Release);
        if self.reader_parked().load(Ordering::Acquire) != 0 {
            platform::wake_handle(&self.reader_wait, self.reader_notify());
        }

        Ok(())
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_target(&self) -> WaitTarget {
        WaitTarget::new(
            self.writer_wait.clone(),
            self.writer_notify() as *const AtomicU32,
            self.writer_parked() as *const AtomicU32,
        )
    }

    pub(crate) fn try_peek_or_closed(&self) -> Result<Option<PeekedMessage>> {
        match self.try_peek_message()? {
            TryPeekResult::Ready(message) => Ok(Some(message)),
            TryPeekResult::Empty => match self.check_channel_active() {
                Ok(()) => Ok(None),
                Err(Error::ChannelClosed) => match self.try_peek_message()? {
                    TryPeekResult::Ready(message) => Ok(Some(message)),
                    TryPeekResult::Empty => Err(Error::ChannelClosed),
                },
                Err(err) => Err(err),
            },
        }
    }

    pub(crate) fn check_channel_active(&self) -> Result<()> {
        match ChannelState::from_u32(self.state().load(Ordering::Acquire)) {
            Some(state) if state.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }

    fn wait_for_message(
        &mut self,
        wait: &impl WaitStrategy,
        timeout: Option<Duration>,
    ) -> Result<PeekedMessage> {
        wait.wait_until(
            &self.writer_wait,
            self.writer_notify(),
            self.writer_parked(),
            timeout,
            || self.try_peek_or_closed(),
        )
    }

    fn read_msg_header(&self, cursor: u64) -> (usize, u32) {
        let mut header = [0u8; MSG_HEADER_SIZE];
        self.read_ring(cursor, &mut header);
        let payload_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let seq = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        (payload_len, seq)
    }

    fn validate_seq(&mut self, seq: u32) -> Result<()> {
        if seq != self.expected_seq {
            return Err(Error::SequenceMismatch {
                expected: self.expected_seq,
                got: seq,
            });
        }
        self.expected_seq = self.expected_seq.wrapping_add(1);
        Ok(())
    }

    fn read_ring(&self, cursor: u64, buf: &mut [u8]) {
        let start = (cursor as usize) & self.ring_mask;
        let first_len = buf.len().min(self.ring_data_size - start);
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.base.add(self.data_offset + start),
                buf.as_mut_ptr(),
                first_len,
            );
            if first_len < buf.len() {
                std::ptr::copy_nonoverlapping(
                    self.base.add(self.data_offset),
                    buf.as_mut_ptr().add(first_len),
                    buf.len() - first_len,
                );
            }
        }
    }

    fn write_cursor(&self) -> &AtomicU64 {
        unsafe { &(*self.ring_header).writer.write_cursor }
    }

    fn read_cursor(&self) -> &AtomicU64 {
        unsafe { &(*self.ring_header).reader.read_cursor }
    }

    fn writer_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.notify }
    }

    fn writer_parked(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.parked }
    }

    fn reader_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.notify }
    }

    fn reader_parked(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.parked }
    }

    fn state(&self) -> &AtomicU32 {
        unsafe { &(*self.global_header).state }
    }
}
