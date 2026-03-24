use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::header::{ChannelState, GlobalHeader, RingHeader};
use crate::platform;
use crate::wait::WaitStrategy;

/// デフォルトのリングデータサイズ (16 MiB)
pub const DEFAULT_RING_DATA_SIZE: usize = 16 * 1024 * 1024;
/// メッセージヘッダ: 4 bytes (length) + 4 bytes (sequence number)
pub const MSG_HEADER_SIZE: usize = 8;

// =========================================================================
// RingSender
// =========================================================================

pub struct RingSender {
    base: *mut u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize,
    next_seq: u32,
    // 最適化 #3: 自分のカーソルをローカルにキャッシュ
    // write_cursor は自分だけが書くので Atomic load 不要
    cached_wc: u64,
    // peer の read_cursor のキャッシュ。空きが足りないときだけ更新
    cached_rc: u64,
}

unsafe impl Send for RingSender {}

impl RingSender {
    pub unsafe fn new(
        base: *mut u8,
        ring_header: *const RingHeader,
        global_header: *const GlobalHeader,
        data_offset: usize,
        ring_data_size: usize,
    ) -> Self {
        debug_assert!(ring_data_size.is_power_of_two());
        Self {
            base,
            ring_header,
            global_header,
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
        timeout: Option<std::time::Duration>,
    ) -> Result<()> {
        self.check_channel_active()?;

        let msg_len = MSG_HEADER_SIZE + payload.len();
        if msg_len > self.ring_data_size {
            return Err(Error::MessageTooLarge {
                size: payload.len(),
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        let wc = self.cached_wc;

        // 最適化 #3: キャッシュ済み rc でまず空きをチェック
        let free = self.ring_data_size as u64 - (wc - self.cached_rc);
        if (free as usize) < msg_len {
            // キャッシュが古い → shared memory から最新 rc を取得して待機
            // borrow checker のため、self のフィールドを個別に参照
            let ring_header = self.ring_header;
            let global_header = self.global_header;
            let ring_data_size = self.ring_data_size;
            let read_cursor = unsafe { &(*ring_header).reader.read_cursor };
            let reader_notify = unsafe { &(*ring_header).reader.notify };
            let reader_parked = unsafe { &(*ring_header).reader.parked };
            let state = unsafe { &(*global_header).state };

            let new_rc = wait.wait_until(reader_notify, reader_parked, timeout, || {
                let rc = read_cursor.load(Ordering::Acquire);
                let free = ring_data_size as u64 - (wc - rc);
                if free >= msg_len as u64 {
                    Ok(Some(rc))
                } else {
                    let s = state.load(Ordering::Acquire);
                    match ChannelState::from_u32(s) {
                        Some(s) if s.is_active() => Ok(None),
                        _ => Err(Error::ChannelClosed),
                    }
                }
            })?;
            self.cached_rc = new_rc;
        }

        // フレームを書き込む
        let len_bytes = (payload.len() as u32).to_le_bytes();
        let seq_bytes = self.next_seq.to_le_bytes();
        self.write_ring(wc, &len_bytes);
        self.write_ring(wc + 4, &seq_bytes);
        self.write_ring(wc + MSG_HEADER_SIZE as u64, payload);

        // write_cursor を進める (Release)
        let new_wc = wc + msg_len as u64;
        self.write_cursor().store(new_wc, Ordering::Release);
        self.cached_wc = new_wc;
        self.next_seq = self.next_seq.wrapping_add(1);

        // 最適化 #1: notify bump + parked のときだけ wake
        self.writer_notify().fetch_add(1, Ordering::Release);
        wait.wake_if_parked(self.writer_notify(), self.writer_parked());

        Ok(())
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
    fn writer_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.notify }
    }
    fn writer_parked(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.parked }
    }

    fn check_channel_active(&self) -> Result<()> {
        let state = unsafe { (*self.global_header).state.load(Ordering::Acquire) };
        match ChannelState::from_u32(state) {
            Some(s) if s.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }
}

// =========================================================================
// RingReceiver
// =========================================================================

pub struct RingReceiver {
    base: *const u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize,
    expected_seq: u32,
    // 最適化 #3: 自分の read_cursor をローカルにキャッシュ
    cached_rc: u64,
}

unsafe impl Send for RingReceiver {}

impl RingReceiver {
    pub unsafe fn new(
        base: *const u8,
        ring_header: *const RingHeader,
        global_header: *const GlobalHeader,
        data_offset: usize,
        ring_data_size: usize,
    ) -> Self {
        debug_assert!(ring_data_size.is_power_of_two());
        Self {
            base,
            ring_header,
            global_header,
            data_offset,
            ring_data_size,
            ring_mask: ring_data_size - 1,
            expected_seq: 1,
            cached_rc: 0,
        }
    }

    pub fn recv(
        &mut self,
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<Vec<u8>> {
        let (rc, data_len) = self.wait_for_message(wait, timeout)?;

        let mut buf = vec![0u8; data_len];
        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf);

        self.advance_cursor(rc, data_len, wait);
        Ok(buf)
    }

    pub fn recv_into(
        &mut self,
        buf: &mut [u8],
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<usize> {
        let (rc, data_len) = self.wait_for_message(wait, timeout)?;

        if data_len > buf.len() {
            self.advance_cursor(rc, data_len, wait);
            return Err(Error::MessageTooLarge {
                size: data_len,
                max: buf.len(),
            });
        }

        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf[..data_len]);
        self.advance_cursor(rc, data_len, wait);
        Ok(data_len)
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        let wc = self.write_cursor().load(Ordering::Acquire);
        let rc = self.cached_rc;

        if wc - rc < MSG_HEADER_SIZE as u64 {
            return Ok(None);
        }

        let (data_len, seq) = self.read_msg_header(rc);

        if data_len > self.ring_data_size - MSG_HEADER_SIZE {
            return Err(Error::MessageTooLarge {
                size: data_len,
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        // 最適化 #2: write_cursor が進んだ = フレーム全体が書き込み済み
        // 2段階チェック不要。ただし部分フレームは corruption として扱う
        let msg_len = MSG_HEADER_SIZE + data_len;
        if wc - rc < msg_len as u64 {
            return Ok(None);
        }

        self.validate_seq(seq)?;

        let mut buf = vec![0u8; data_len];
        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf);

        let new_rc = rc + msg_len as u64;
        self.read_cursor().store(new_rc, Ordering::Release);
        self.cached_rc = new_rc;
        self.reader_notify().fetch_add(1, Ordering::Release);
        // try_recv は非ブロッキングなので wake は常に呼ぶ (parked チェック省略)
        platform::futex_wake(self.reader_notify());

        Ok(Some(buf))
    }

    /// 最適化 #2: 1段階 wait (write_cursor が進んだ = フレーム完成)
    ///
    /// writer は header + payload を全て書いてから write_cursor を Release store する。
    /// よって write_cursor - read_cursor >= MSG_HEADER_SIZE ならフレーム全体が見える。
    /// 2段階目の "payload 全体が来るまで待つ" は不要。
    fn wait_for_message(
        &mut self,
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<(u64, usize)> {
        let rc = self.cached_rc;

        // write_cursor が十分進むまで待つ (1段階のみ)
        let wc = wait.wait_until(self.writer_notify(), self.writer_parked(), timeout, || {
            let wc = self.write_cursor().load(Ordering::Acquire);
            if wc - rc >= MSG_HEADER_SIZE as u64 {
                Ok(Some(wc))
            } else {
                self.check_channel_active()?;
                Ok(None)
            }
        })?;

        let (data_len, seq) = self.read_msg_header(rc);

        if data_len > self.ring_data_size - MSG_HEADER_SIZE {
            return Err(Error::MessageTooLarge {
                size: data_len,
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        // フレーム全体が見えるか検証 (write_cursor はフレーム完成後に進むので通常は成立)
        let msg_len = (MSG_HEADER_SIZE + data_len) as u64;
        debug_assert!(
            wc - rc >= msg_len,
            "partial frame visible: wc={wc}, rc={rc}, msg_len={msg_len}"
        );

        self.validate_seq(seq)?;
        Ok((rc, data_len))
    }

    /// read_cursor を進めて writer を (必要なら) 起こす
    fn advance_cursor(&mut self, rc: u64, data_len: usize, wait: &impl WaitStrategy) {
        let new_rc = rc + (MSG_HEADER_SIZE + data_len) as u64;
        self.read_cursor().store(new_rc, Ordering::Release);
        self.cached_rc = new_rc;

        self.reader_notify().fetch_add(1, Ordering::Release);
        wait.wake_if_parked(self.reader_notify(), self.reader_parked());
    }

    fn read_msg_header(&self, cursor: u64) -> (usize, u32) {
        let mut header = [0u8; MSG_HEADER_SIZE];
        self.read_ring(cursor, &mut header);
        let data_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let seq = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        (data_len, seq)
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

    fn check_channel_active(&self) -> Result<()> {
        let state = unsafe { (*self.global_header).state.load(Ordering::Acquire) };
        match ChannelState::from_u32(state) {
            Some(s) if s.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }
}
