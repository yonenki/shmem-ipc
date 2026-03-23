use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::header::{ChannelState, GlobalHeader, RingHeader};
use crate::platform;
use crate::wait::WaitStrategy;

/// デフォルトのリングデータサイズ (16 MiB)
pub const DEFAULT_RING_DATA_SIZE: usize = 16 * 1024 * 1024;
/// メッセージヘッダ: 4 bytes (length) + 4 bytes (sequence number)
pub const MSG_HEADER_SIZE: usize = 8;

/// SPSC リングバッファの送信側
///
/// 1つのリング方向に対して1つの RingSender が存在する。
/// Send を実装するが Sync は実装しない (SPSC: 1 writer のみ)。
pub struct RingSender {
    base: *mut u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize, // ring_data_size - 1 (2の累乗前提でモジュロを高速化)
    next_seq: u32,
}

unsafe impl Send for RingSender {}

/// SPSC リングバッファの受信側
///
/// Send を実装するが Sync は実装しない (SPSC: 1 reader のみ)。
pub struct RingReceiver {
    base: *const u8,
    ring_header: *const RingHeader,
    global_header: *const GlobalHeader,
    data_offset: usize,
    ring_data_size: usize,
    ring_mask: usize,
    expected_seq: u32,
}

unsafe impl Send for RingReceiver {}

impl RingSender {
    /// RingSender を構築する
    ///
    /// # Safety
    /// - base は有効な mmap ポインタで、ring_header と data_offset の範囲が mmap 内に収まること
    /// - ring_data_size は 2 の累乗であること
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
        }
    }

    /// メッセージを送信する
    ///
    /// payload を [length:u32 LE][seq:u32 LE][payload] のフレームとしてリングに書き込む。
    /// リングに空きがなければ WaitStrategy で待機する。
    pub fn send(
        &mut self,
        payload: &[u8],
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<()> {
        // close 済みチャネルへの送信を拒否
        // (drain セマンティクスは recv 側のみ — send は即エラー)
        self.check_channel_active()?;

        let msg_len = MSG_HEADER_SIZE + payload.len();
        if msg_len > self.ring_data_size {
            return Err(Error::MessageTooLarge {
                size: payload.len(),
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        // リングに空きができるまで待機
        // 空きを先にチェックし、空きがないときだけ close を検出
        let wc = wait.wait_until(
            self.reader_notify(),
            timeout,
            || {
                let wc = self.write_cursor().load(Ordering::Relaxed);
                let rc = self.read_cursor().load(Ordering::Acquire);
                let free = self.ring_data_size as u64 - (wc - rc);
                if free >= msg_len as u64 {
                    Ok(Some(wc))
                } else {
                    self.check_channel_active()?;
                    Ok(None)
                }
            },
        )?;

        // フレームを書き込む
        let len_bytes = (payload.len() as u32).to_le_bytes();
        let seq_bytes = self.next_seq.to_le_bytes();
        self.write_ring(wc, &len_bytes);
        self.write_ring(wc + 4, &seq_bytes);
        self.write_ring(wc + MSG_HEADER_SIZE as u64, payload);

        // write_cursor を進める (Release: 上のデータ書き込みが先に見えることを保証)
        self.write_cursor()
            .store(wc + msg_len as u64, Ordering::Release);
        self.next_seq = self.next_seq.wrapping_add(1);

        // reader を起こす
        self.writer_notify().fetch_add(1, Ordering::Release);
        platform::futex_wake(self.writer_notify());

        Ok(())
    }

    /// wrap-around 対応のリング書き込み
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

    /// writer 側の notify (reader がこれを watch して起きる)
    fn writer_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).writer.notify }
    }

    /// reader 側の notify (writer がこれを watch して空きを待つ)
    fn reader_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.notify }
    }

    fn check_channel_active(&self) -> Result<()> {
        let state = unsafe { (*self.global_header).state.load(Ordering::Acquire) };
        match ChannelState::from_u32(state) {
            Some(s) if s.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }
}

impl RingReceiver {
    /// RingReceiver を構築する
    ///
    /// # Safety
    /// - base は有効な mmap ポインタで、ring_header と data_offset の範囲が mmap 内に収まること
    /// - ring_data_size は 2 の累乗であること
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
        }
    }

    /// メッセージを受信する (Vec<u8> を返す)
    pub fn recv(
        &mut self,
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<Vec<u8>> {
        let (rc, data_len) = self.wait_for_message(wait, timeout)?;

        let mut buf = vec![0u8; data_len];
        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf);

        // read_cursor を進める
        let msg_len = (MSG_HEADER_SIZE + data_len) as u64;
        self.read_cursor().store(rc + msg_len, Ordering::Release);

        // writer を起こす (空きができたことを通知)
        self.reader_notify().fetch_add(1, Ordering::Release);
        platform::futex_wake(self.reader_notify());

        Ok(buf)
    }

    /// 呼び出し元のバッファに受信する
    ///
    /// バッファサイズがメッセージより小さい場合は MessageTooLarge エラーを返す。
    /// サイレントな切り詰めは行わない。
    pub fn recv_into(
        &mut self,
        buf: &mut [u8],
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<usize> {
        let (rc, data_len) = self.wait_for_message(wait, timeout)?;

        if data_len > buf.len() {
            // カーソルを進めてメッセージを消費する (さもないとリングが詰まる)
            let msg_len = (MSG_HEADER_SIZE + data_len) as u64;
            self.read_cursor().store(rc + msg_len, Ordering::Release);
            self.reader_notify().fetch_add(1, Ordering::Release);
            platform::futex_wake(self.reader_notify());
            return Err(Error::MessageTooLarge {
                size: data_len,
                max: buf.len(),
            });
        }

        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf[..data_len]);

        let msg_len = (MSG_HEADER_SIZE + data_len) as u64;
        self.read_cursor().store(rc + msg_len, Ordering::Release);

        self.reader_notify().fetch_add(1, Ordering::Release);
        platform::futex_wake(self.reader_notify());

        Ok(data_len)
    }

    /// ノンブロッキング受信
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        let wc = self.write_cursor().load(Ordering::Acquire);
        let rc = self.read_cursor().load(Ordering::Relaxed);

        if wc - rc < MSG_HEADER_SIZE as u64 {
            return Ok(None);
        }

        let (data_len, seq) = self.read_msg_header(rc);

        let msg_len = MSG_HEADER_SIZE + data_len;
        if wc - rc < msg_len as u64 {
            return Ok(None); // ヘッダは見えるがペイロードがまだ
        }

        self.validate_seq(seq)?;

        let mut buf = vec![0u8; data_len];
        self.read_ring(rc + MSG_HEADER_SIZE as u64, &mut buf);

        self.read_cursor()
            .store(rc + msg_len as u64, Ordering::Release);
        self.reader_notify().fetch_add(1, Ordering::Release);
        platform::futex_wake(self.reader_notify());

        Ok(Some(buf))
    }

    /// メッセージヘッダ (length + seq) が来るまで待ち、ペイロード全体が揃うまで待つ
    fn wait_for_message(
        &mut self,
        wait: &impl WaitStrategy,
        timeout: Option<std::time::Duration>,
    ) -> Result<(u64, usize)> {
        // Phase 1: length ヘッダが来るまで待つ
        //
        // データの存在を先にチェックし、close 状態はリングが空のときだけ返す。
        // これにより、peer が send 直後に drop してもバッファ済みデータを受信できる。
        let rc = wait.wait_until(
            self.writer_notify(),
            timeout,
            || {
                let wc = self.write_cursor().load(Ordering::Acquire);
                let rc = self.read_cursor().load(Ordering::Relaxed);
                if wc - rc >= MSG_HEADER_SIZE as u64 {
                    Ok(Some(rc))
                } else {
                    // リングが空の場合のみ close を検出
                    self.check_channel_active()?;
                    Ok(None)
                }
            },
        )?;

        // length + seq を読む
        let (data_len, seq) = self.read_msg_header(rc);
        let msg_len = MSG_HEADER_SIZE + data_len;

        // data_len の妥当性チェック: リングサイズを超える値は破損データ
        if data_len > self.ring_data_size - MSG_HEADER_SIZE {
            return Err(Error::MessageTooLarge {
                size: data_len,
                max: self.ring_data_size - MSG_HEADER_SIZE,
            });
        }

        // Phase 2: ペイロード全体が来るまで待つ
        wait.wait_until(
            self.writer_notify(),
            timeout,
            || {
                let wc = self.write_cursor().load(Ordering::Acquire);
                if wc - rc >= msg_len as u64 {
                    Ok(Some(()))
                } else {
                    self.check_channel_active()?;
                    Ok(None)
                }
            },
        )?;

        self.validate_seq(seq)?;

        Ok((rc, data_len))
    }

    /// メッセージヘッダ (length + sequence) を読む
    fn read_msg_header(&self, cursor: u64) -> (usize, u32) {
        let mut header = [0u8; MSG_HEADER_SIZE];
        self.read_ring(cursor, &mut header);
        let data_len = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let seq = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        (data_len, seq)
    }

    /// シーケンス番号を検証する
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

    /// wrap-around 対応のリング読み出し
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

    fn reader_notify(&self) -> &AtomicU32 {
        unsafe { &(*self.ring_header).reader.notify }
    }

    fn check_channel_active(&self) -> Result<()> {
        let state = unsafe { (*self.global_header).state.load(Ordering::Acquire) };
        match ChannelState::from_u32(state) {
            Some(s) if s.is_active() => Ok(()),
            _ => Err(Error::ChannelClosed),
        }
    }
}
