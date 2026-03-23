use std::io::{Read, Write};
use std::time::Duration;

use crate::channel::{Channel, ChannelConfig};
use crate::connection::ShmemConnection;
use crate::error::{Error, Result};

/// ハンドシェイク用ソケットのパス
fn handshake_path(name: &str) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::path::PathBuf::from(format!("/tmp/shmem_ipc_{name}.sock"))
    }
    #[cfg(windows)]
    {
        // Windows Named Pipe はファイルパスではないが、
        // ここでは pipe 名を返す
        std::path::PathBuf::from(format!(r"\\.\pipe\shmem_ipc_{name}"))
    }
}

// =========================================================================
// Unix 実装
// =========================================================================

#[cfg(unix)]
mod imp {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        name: String,
        config: ChannelConfig,
        counter: u64,
    }

    impl Listener {
        pub fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
            let path = handshake_path(name);
            // 既存ソケットファイルを削除
            let _ = std::fs::remove_file(&path);
            let inner = UnixListener::bind(&path)?;
            Ok(Self {
                inner,
                name: name.to_string(),
                config,
                counter: 0,
            })
        }

        pub fn accept(&mut self) -> Result<ShmemConnection> {
            let (stream, _addr) = self.inner.accept()?;
            self.handshake(stream)
        }

        pub fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection> {
            // nonblocking + ポーリングでタイムアウトを実現
            self.inner.set_nonblocking(true)?;
            let deadline = std::time::Instant::now() + timeout;
            let result = loop {
                match self.inner.accept() {
                    Ok((stream, _addr)) => {
                        stream.set_nonblocking(false)?;
                        break Ok(stream);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break Err(Error::TimedOut);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => break Err(e.into()),
                }
            };
            self.inner.set_nonblocking(false)?;
            self.handshake(result?)
        }

        fn handshake(&mut self, mut stream: UnixStream) -> Result<ShmemConnection> {
            // 1. ユニークな接続名を生成
            let conn_name = format!("{}.conn.{}", self.name, self.counter);
            self.counter += 1;

            // 2. データ Channel を Server として作成
            let channel = Channel::create_with_config(&conn_name, self.config.clone())?;

            // 3. 接続名をクライアントに送信
            //    フォーマット: [len:u16 LE][name bytes]
            let name_bytes = conn_name.as_bytes();
            let len = name_bytes.len() as u16;
            stream.write_all(&len.to_le_bytes())?;
            stream.write_all(name_bytes)?;
            stream.flush()?;

            // 4. Channel を ShmemConnection に変換
            Ok(channel.into_connection())
        }

        pub fn cleanup(name: &str) {
            let path = handshake_path(name);
            let _ = std::fs::remove_file(path);
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let path = handshake_path(&self.name);
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn connect_to(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
        let path = handshake_path(name);

        // Server の listen を待つためリトライ
        let deadline = std::time::Instant::now() + config.connect_timeout;
        let stream = loop {
            match UnixStream::connect(&path) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(Error::Io(e)),
            }
        };

        // 接続名を受信
        let conn_name = read_conn_name(stream)?;

        // データ Channel に Client として接続
        let channel = Channel::open_with_config(&conn_name, config)?;
        Ok(channel.into_connection())
    }

    fn read_conn_name(mut stream: UnixStream) -> Result<String> {
        let mut len_buf = [0u8; 2];
        stream.read_exact(&mut len_buf)?;
        let len = u16::from_le_bytes(len_buf) as usize;

        let mut name_buf = vec![0u8; len];
        stream.read_exact(&mut name_buf)?;

        String::from_utf8(name_buf)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }
}

// =========================================================================
// Windows 実装 (stub — 将来 Named Pipe で実装)
// =========================================================================

#[cfg(windows)]
mod imp {
    use super::*;

    pub struct Listener {
        name: String,
        config: ChannelConfig,
        counter: u64,
    }

    impl Listener {
        pub fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
            // TODO: CreateNamedPipeW で listen
            todo!("Windows listener not yet implemented")
        }

        pub fn accept(&mut self) -> Result<ShmemConnection> {
            todo!("Windows accept not yet implemented")
        }

        pub fn accept_timeout(&mut self, _timeout: Duration) -> Result<ShmemConnection> {
            todo!("Windows accept_timeout not yet implemented")
        }

        pub fn cleanup(_name: &str) {}
    }

    impl Drop for Listener {
        fn drop(&mut self) {}
    }

    pub fn connect_to(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
        todo!("Windows connect not yet implemented")
    }
}

// =========================================================================
// Public API
// =========================================================================

/// 複数クライアントからの接続を受け付ける Listener
///
/// ハンドシェイクに Unix Socket (Linux) / Named Pipe (Windows) を使い、
/// データ転送は SharedMem Channel で行う。
///
/// ```rust,no_run
/// use shmem_ipc::{ShmemListener, ChannelConfig};
///
/// let mut listener = ShmemListener::bind("my-service", ChannelConfig::default()).unwrap();
/// loop {
///     let conn = listener.accept().unwrap();
///     let (tx, rx) = conn.split();
///     // tx, rx を別スレッドに渡して通信
/// }
/// ```
pub struct ShmemListener(imp::Listener);

impl ShmemListener {
    pub fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
        Ok(Self(imp::Listener::bind(name, config)?))
    }

    /// 新しいクライアント接続を受け付ける (ブロッキング)
    pub fn accept(&mut self) -> Result<ShmemConnection> {
        self.0.accept()
    }

    /// 新しいクライアント接続を受け付ける (タイムアウト付き)
    pub fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection> {
        self.0.accept_timeout(timeout)
    }

    /// リソースをクリーンアップする
    pub fn cleanup(name: &str) {
        imp::Listener::cleanup(name);
    }
}

/// サーバーに接続する
///
/// ハンドシェイク後、SharedMem Channel 上の ShmemConnection を返す。
pub fn connect(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
    imp::connect_to(name, config)
}
