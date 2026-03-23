use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Duration;

use memmap2::MmapMut;

use crate::error::{Error, Result};
use crate::header::{ChannelState, GlobalHeader, RingHeader, RingOffsets, MAGIC, VERSION};
use crate::platform;
use crate::ring::{RingReceiver, RingSender, DEFAULT_RING_DATA_SIZE};
use crate::wait::SpinThenWait;

/// チャネルの役割
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// チャネルの設定
#[derive(Clone)]
pub struct ChannelConfig {
    /// リングバッファのデータ領域サイズ (2の累乗必須)。デフォルト: 16 MiB
    pub ring_size: usize,
    /// 待機戦略。デフォルト: SpinThenWait { spin_count: 256 }
    pub wait_strategy: SpinThenWait,
    /// Server 待ちのタイムアウト (Client::open 時)。デフォルト: 5秒
    pub connect_timeout: Duration,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            ring_size: DEFAULT_RING_DATA_SIZE,
            wait_strategy: SpinThenWait::default(),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// 双方向の共有メモリ IPC チャネル
///
/// 内部に2本の SPSC リングバッファ (Server→Client, Client→Server) を持ち、
/// それぞれ RingSender / RingReceiver で送受信する。
pub struct Channel {
    mmap: MmapMut,
    sender: RingSender,
    receiver: RingReceiver,
    role: Role,
    name: String,
    wait: SpinThenWait,
}

impl Channel {
    /// Server としてチャネルを作成する
    ///
    /// 共有メモリファイルを作成し、ヘッダを初期化して Client の接続を待てる状態にする。
    pub fn create(name: &str) -> Result<Self> {
        Self::create_with_config(name, ChannelConfig::default())
    }

    pub fn create_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        if !config.ring_size.is_power_of_two() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "ring_size must be a power of two",
            )));
        }

        let path = platform::shm_path(name);
        let offsets = RingOffsets::new(config.ring_size);

        // ファイルを作成して mmap
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.set_len(offsets.total_size as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        // GlobalHeader を初期化
        let base = mmap.as_ptr() as *mut u8;
        let gh = unsafe { &mut *(base as *mut GlobalHeader) };
        gh.magic = MAGIC;
        gh.version = VERSION;
        gh.ring_data_size = config.ring_size as u32;
        gh.server_pid = platform::current_pid();
        gh.client_pid.store(0, Ordering::Release);
        gh.server_heartbeat.store(0, Ordering::Release);
        gh.client_heartbeat.store(0, Ordering::Release);

        // RingHeader を初期化 (全カーソル・notify を 0 に)
        for ring_header_offset in [offsets.ring_a_header, offsets.ring_b_header] {
            let rh = unsafe { &mut *(base.add(ring_header_offset) as *mut RingHeader) };
            rh.writer.write_cursor.store(0, Ordering::Release);
            rh.writer.notify.store(0, Ordering::Release);
            rh.writer.parked.store(0, Ordering::Release);
            rh.reader.read_cursor.store(0, Ordering::Release);
            rh.reader.notify.store(0, Ordering::Release);
            rh.reader.parked.store(0, Ordering::Release);
        }

        // state を ServerReady にする (Release: 上の全初期化が先に見えることを保証)
        gh.state
            .store(ChannelState::ServerReady as u32, Ordering::Release);

        // Server: Ring A に書く、Ring B から読む
        let global_header_ptr = base as *const GlobalHeader;
        let sender = unsafe {
            RingSender::new(
                base,
                base.add(offsets.ring_a_header) as *const RingHeader,
                global_header_ptr,
                offsets.ring_a_data,
                config.ring_size,
            )
        };
        let receiver = unsafe {
            RingReceiver::new(
                base as *const u8,
                base.add(offsets.ring_b_header) as *const RingHeader,
                global_header_ptr,
                offsets.ring_b_data,
                config.ring_size,
            )
        };

        Ok(Self {
            mmap,
            sender,
            receiver,
            role: Role::Server,
            name: name.to_string(),
            wait: config.wait_strategy,
        })
    }

    /// Client としてチャネルに接続する
    ///
    /// Server が作成した共有メモリファイルを開き、ヘッダを検証して接続する。
    pub fn open(name: &str) -> Result<Self> {
        Self::open_with_config(name, ChannelConfig::default())
    }

    pub fn open_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        let path = platform::shm_path(name);
        let offsets = RingOffsets::new(config.ring_size);

        // Server がファイルを作るのを待つ
        let deadline = std::time::Instant::now() + config.connect_timeout;
        loop {
            if Path::new(&path).exists() {
                if std::fs::metadata(&path)
                    .map(|m| m.len())
                    .unwrap_or(0)
                    >= offsets.total_size as u64
                {
                    break;
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let file = OpenOptions::new().read(true).write(true).open(&path)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };

        let base = mmap.as_ptr() as *mut u8;
        let gh = unsafe { GlobalHeader::from_ptr(base as *const u8) };

        // state を Acquire で先に読む。
        // Server は magic/version/ring_data_size を plain store した後に
        // state.store(ServerReady, Release) するので、ここで Acquire することで
        // それらの plain store が確実に見える (happens-before 関係の確立)。
        // ARM 等の弱いメモリモデルでは、この順序が正しさに必須。
        let current_state = gh.state.load(Ordering::Acquire);
        if current_state != ChannelState::ServerReady as u32 {
            return Err(Error::ChannelClosed);
        }

        // Acquire の後なので plain fields は確実に最新値が見える
        gh.validate()?;

        if gh.ring_data_size as usize != config.ring_size {
            return Err(Error::InvalidHeader);
        }

        // state を ServerReady → Connected に CAS
        let state_result = gh.state.compare_exchange(
            ChannelState::ServerReady as u32,
            ChannelState::Connected as u32,
            Ordering::AcqRel,
            Ordering::Acquire,
        );

        if state_result.is_err() {
            return Err(Error::ChannelClosed);
        }

        // Client PID を記録
        gh.client_pid
            .store(platform::current_pid(), Ordering::Release);

        // Client: Ring B に書く、Ring A から読む (Server の逆)
        let global_header_ptr = base as *const GlobalHeader;
        let sender = unsafe {
            RingSender::new(
                base,
                base.add(offsets.ring_b_header) as *const RingHeader,
                global_header_ptr,
                offsets.ring_b_data,
                config.ring_size,
            )
        };
        let receiver = unsafe {
            RingReceiver::new(
                base as *const u8,
                base.add(offsets.ring_a_header) as *const RingHeader,
                global_header_ptr,
                offsets.ring_a_data,
                config.ring_size,
            )
        };

        Ok(Self {
            mmap,
            sender,
            receiver,
            role: Role::Client,
            name: name.to_string(),
            wait: config.wait_strategy,
        })
    }

    /// メッセージを送信する (ブロッキング、タイムアウトなし)
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender.send(payload, &self.wait, None)
    }

    /// メッセージを送信する (タイムアウト付き)
    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender.send(payload, &self.wait, Some(timeout))
    }

    /// メッセージを受信する (ブロッキング、タイムアウトなし)
    pub fn recv(&mut self) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, None)
    }

    /// メッセージを受信する (タイムアウト付き)
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, Some(timeout))
    }

    /// 呼び出し元のバッファに受信する
    pub fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.receiver.recv_into(buf, &self.wait, None)
    }

    /// ノンブロッキング受信
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.try_recv()
    }

    /// チャネルの役割を取得する
    pub fn role(&self) -> Role {
        self.role
    }

    /// チャネルの現在の状態を取得する
    pub fn state(&self) -> ChannelState {
        let v = self.global_header().state.load(Ordering::Acquire);
        ChannelState::from_u32(v).unwrap_or(ChannelState::Closed)
    }

    /// 残ったリソースを手動で削除する (SIGKILL 後のクリーンアップ用)
    pub fn cleanup(name: &str) -> Result<()> {
        let path = platform::shm_path(name);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Channel を ShmemConnection に変換する
    pub fn into_connection(self) -> crate::connection::ShmemConnection {
        let is_server = self.role == Role::Server;
        let name = self.name.clone();
        let wait = SpinThenWait { spin_count: self.wait.spin_count };

        // Drop を防ぐために分解
        let mmap = unsafe { std::ptr::read(&self.mmap) };
        let sender = unsafe { std::ptr::read(&self.sender) };
        let receiver = unsafe { std::ptr::read(&self.receiver) };
        std::mem::forget(self);

        crate::connection::ShmemConnection::new(mmap, sender, receiver, wait, name, is_server)
    }

    fn global_header(&self) -> &GlobalHeader {
        unsafe { GlobalHeader::from_ptr(self.mmap.as_ptr()) }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        // state を Closed に設定 (peer に通知)
        self.global_header()
            .state
            .store(ChannelState::Closed as u32, Ordering::Release);

        // peer の futex を起こす (spin-wait やfutex_wait から抜けさせる)
        // Ring A, Ring B 両方の writer/reader notify を wake
        let offsets = RingOffsets::new(self.global_header().ring_data_size as usize);
        let base = self.mmap.as_ptr() as *mut u8;
        for ring_header_offset in [offsets.ring_a_header, offsets.ring_b_header] {
            let rh = unsafe { &*(base.add(ring_header_offset) as *const RingHeader) };
            rh.writer.notify.fetch_add(1, Ordering::Release);
            platform::futex_wake(&rh.writer.notify);
            rh.reader.notify.fetch_add(1, Ordering::Release);
            platform::futex_wake(&rh.reader.notify);
        }

        // Server のみ共有メモリファイルを削除する
        // (Client が先に drop しても Server のファイルは残る)
        if self.role == Role::Server {
            let path = platform::shm_path(&self.name);
            let _ = std::fs::remove_file(path);
        }

        // mmap は MmapMut の Drop で自動的に unmap される
    }
}
