use std::fmt;

/// shmem-ipc のエラー型
#[derive(Debug)]
pub enum Error {
    /// 操作がタイムアウトした
    TimedOut,
    /// 相手プロセスが切断された (クラッシュまたは正常終了)
    PeerDisconnected,
    /// メッセージがリングバッファの容量を超えている
    MessageTooLarge { size: usize, max: usize },
    /// チャネルが閉じられた
    ChannelClosed,
    /// 共有メモリファイルのヘッダが不正 (magic/version 不一致)
    InvalidHeader,
    /// シーケンス番号の不一致 (データ破損またはメッセージ欠落)
    SequenceMismatch { expected: u32, got: u32 },
    /// OS レベルの I/O エラー
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TimedOut => write!(f, "operation timed out"),
            Error::PeerDisconnected => write!(f, "peer process disconnected"),
            Error::MessageTooLarge { size, max } => {
                write!(f, "message too large: {size} bytes (max {max})")
            }
            Error::ChannelClosed => write!(f, "channel closed"),
            Error::InvalidHeader => write!(f, "invalid shared memory header"),
            Error::SequenceMismatch { expected, got } => {
                write!(f, "sequence mismatch: expected {expected}, got {got}")
            }
            Error::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
