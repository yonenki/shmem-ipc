use std::sync::atomic::{AtomicU32, AtomicU64};

use crate::error::{Error, Result};

/// 共有メモリファイルのマジックナンバー ("SHMC")
pub const MAGIC: u32 = 0x5348_4D43;
/// プロトコルバージョン
pub const VERSION: u32 = 1;
/// GlobalHeader のサイズ (256 bytes に固定、将来の拡張余地を残す)
pub const GLOBAL_HEADER_SIZE: usize = 256;
/// RingHeader のサイズ (128 bytes = cache line 2本分)
pub const RING_HEADER_SIZE: usize = 128;

/// チャネルの状態遷移
///
/// ```text
/// Init(0) → ServerReady(1) → Connected(2) → Closing(3) → Closed(4)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ChannelState {
    /// 初期化中
    Init = 0,
    /// Server がヘッダを初期化完了、Client の接続待ち
    ServerReady = 1,
    /// Client が接続済み、通信可能
    Connected = 2,
    /// どちらかが切断処理を開始
    Closing = 3,
    /// 切断完了
    Closed = 4,
}

impl ChannelState {
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Init),
            1 => Some(Self::ServerReady),
            2 => Some(Self::Connected),
            3 => Some(Self::Closing),
            4 => Some(Self::Closed),
            _ => None,
        }
    }

    /// 通信可能な状態か
    pub fn is_active(self) -> bool {
        matches!(self, Self::ServerReady | Self::Connected)
    }
}

/// 共有メモリファイルの先頭に配置されるグローバルヘッダ
///
/// Server が作成時に初期化し、Client が接続時に検証する。
/// align(64) でキャッシュライン境界に配置。
#[repr(C, align(64))]
pub struct GlobalHeader {
    pub magic: u32,
    pub version: u32,
    pub ring_data_size: u32,
    _pad0: u32,
    pub server_pid: u64,
    pub client_pid: AtomicU64,
    pub server_heartbeat: AtomicU64,
    pub client_heartbeat: AtomicU64,
    pub state: AtomicU32,
    // 残りは 256 bytes までパディング
}

impl GlobalHeader {
    /// mmap のベースポインタから GlobalHeader への参照を取得する
    ///
    /// # Safety
    /// - ptr は GLOBAL_HEADER_SIZE 以上の有効なメモリを指すこと
    /// - ptr は 64 byte アラインされていること (mmap はページアラインなので OK)
    pub unsafe fn from_ptr<'a>(ptr: *const u8) -> &'a Self {
        unsafe { &*(ptr as *const Self) }
    }

    /// magic と version を検証する
    pub fn validate(&self) -> Result<()> {
        if self.magic != MAGIC || self.version != VERSION {
            return Err(Error::InvalidHeader);
        }
        Ok(())
    }
}

/// リングバッファの書き込みカーソル行 (64 bytes, cache line 1本分)
///
/// writer のみが write_cursor を更新し、reader は読むだけ。
/// notify は writer が書き込み後に increment する futex ワード。
/// parked は reader が futex_wait に入る直前に 1 にし、抜けたら 0 にする。
/// writer は parked == 1 のときだけ futex_wake を呼ぶ (syscall 省略)。
#[repr(C, align(64))]
pub struct WriteCursorLine {
    pub write_cursor: AtomicU64,
    pub notify: AtomicU32,
    pub parked: AtomicU32,
    // 残りは 64 bytes までパディング
}

/// リングバッファの読み取りカーソル行 (64 bytes, cache line 1本分)
///
/// reader のみが read_cursor を更新し、writer は読むだけ。
/// notify は reader が読み取り後に increment する futex ワード。
/// parked は writer が futex_wait に入る直前に 1 にし、抜けたら 0 にする。
#[repr(C, align(64))]
pub struct ReadCursorLine {
    pub read_cursor: AtomicU64,
    pub notify: AtomicU32,
    pub parked: AtomicU32,
    // 残りは 64 bytes までパディング
}

/// 1方向のリングバッファのヘッダ (128 bytes)
///
/// WriteCursorLine と ReadCursorLine が別キャッシュラインに配置される。
#[repr(C)]
pub struct RingHeader {
    pub writer: WriteCursorLine,
    pub reader: ReadCursorLine,
}

/// リングバッファの各オフセットを計算する
pub struct RingOffsets {
    /// Ring A (Server→Client) のヘッダオフセット
    pub ring_a_header: usize,
    /// Ring A のデータ領域オフセット
    pub ring_a_data: usize,
    /// Ring B (Client→Server) のヘッダオフセット
    pub ring_b_header: usize,
    /// Ring B のデータ領域オフセット
    pub ring_b_data: usize,
    /// 共有メモリファイル全体のサイズ
    pub total_size: usize,
}

impl RingOffsets {
    pub fn new(ring_data_size: usize) -> Self {
        let ring_total = RING_HEADER_SIZE + ring_data_size;
        Self {
            ring_a_header: GLOBAL_HEADER_SIZE,
            ring_a_data: GLOBAL_HEADER_SIZE + RING_HEADER_SIZE,
            ring_b_header: GLOBAL_HEADER_SIZE + ring_total,
            ring_b_data: GLOBAL_HEADER_SIZE + ring_total + RING_HEADER_SIZE,
            total_size: GLOBAL_HEADER_SIZE + ring_total * 2,
        }
    }
}
