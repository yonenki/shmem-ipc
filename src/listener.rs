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
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread::JoinHandle;

    fn pipe_name(name: &str) -> String {
        format!(r"\\.\pipe\shmem_ipc_{name}")
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 未接続の Named Pipe instance を1本作成する
    fn create_pipe_instance(pipe_path: &str) -> Result<File> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows_sys::Win32::System::Pipes::*;

        let wide_name = wide(pipe_path);
        let handle = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }

        Ok(unsafe { File::from_raw_handle(handle as _) })
    }

    /// 既存の Named Pipe instance が client に接続されるまで待つ
    fn wait_for_pipe_client(pipe: File) -> Result<File> {
        use windows_sys::Win32::Foundation::ERROR_PIPE_CONNECTED;
        use windows_sys::Win32::System::Pipes::ConnectNamedPipe;

        let ok = unsafe { ConnectNamedPipe(pipe.as_raw_handle() as _, std::ptr::null_mut()) };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                return Err(err.into());
            }
        }

        Ok(pipe)
    }

    /// Named Pipe に client として接続
    fn connect_pipe(pipe_path: &str, timeout: Duration) -> Result<File> {
        use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING};

        let wide_name = wide(pipe_path);
        let deadline = std::time::Instant::now() + timeout;

        loop {
            let handle = unsafe {
                CreateFileW(
                    wide_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Ok(unsafe { File::from_raw_handle(handle as _) });
            }
            if std::time::Instant::now() >= deadline {
                return Err(Error::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn send_conn_name(pipe: &mut File, name: &str) -> Result<()> {
        let bytes = name.as_bytes();
        let len = bytes.len() as u16;
        pipe.write_all(&len.to_le_bytes())?;
        pipe.write_all(bytes)?;
        pipe.flush()?;
        Ok(())
    }

    fn recv_conn_name(pipe: &mut File) -> Result<String> {
        let mut len_buf = [0u8; 2];
        pipe.read_exact(&mut len_buf)?;
        let len = u16::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        pipe.read_exact(&mut buf)?;
        String::from_utf8(buf)
            .map_err(|e| Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
    }

    struct PendingAccept {
        rx: Receiver<Result<File>>,
        handle: JoinHandle<()>,
    }

    impl PendingAccept {
        fn start(pipe_path: &str) -> Result<Self> {
            let pipe = create_pipe_instance(pipe_path)?;
            let (tx, rx) = mpsc::channel();
            let handle = std::thread::spawn(move || {
                let result = wait_for_pipe_client(pipe);
                let _ = tx.send(result);
            });
            Ok(Self { rx, handle })
        }
    }

    fn accept_thread_disconnected() -> Error {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "named pipe accept thread disconnected",
        ))
    }

    pub struct Listener {
        name: String,
        config: ChannelConfig,
        counter: u64,
        pipe_path: String,
        pending_accept: Option<PendingAccept>,
    }

    impl Listener {
        pub fn bind(name: &str, config: ChannelConfig) -> Result<Self> {
            let pipe_path = pipe_name(name);
            Ok(Self {
                name: name.to_string(),
                config,
                counter: 0,
                pipe_path,
                pending_accept: None,
            })
        }

        pub fn accept(&mut self) -> Result<ShmemConnection> {
            let mut pipe = self.wait_for_pipe()?;
            self.handshake(&mut pipe)
        }

        pub fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection> {
            let mut pipe = self.wait_for_pipe_timeout(timeout)?;
            self.handshake(&mut pipe)
        }

        fn handshake(&mut self, pipe: &mut File) -> Result<ShmemConnection> {
            let conn_name = format!("{}.conn.{}", self.name, self.counter);
            self.counter += 1;

            let channel = Channel::create_with_config(&conn_name, self.config.clone())?;
            send_conn_name(pipe, &conn_name)?;
            Ok(channel.into_connection())
        }

        fn ensure_pending_accept(&mut self) -> Result<()> {
            if self.pending_accept.is_none() {
                self.pending_accept = Some(PendingAccept::start(&self.pipe_path)?);
            }
            Ok(())
        }

        fn prime_next_accept(&mut self) -> Result<()> {
            debug_assert!(self.pending_accept.is_none());
            // Keep one fresh server instance published at all times. Without this,
            // clients racing the next accept can miss the pipe entirely on Windows.
            self.pending_accept = Some(PendingAccept::start(&self.pipe_path)?);
            Ok(())
        }

        fn wait_for_pipe(&mut self) -> Result<File> {
            self.ensure_pending_accept()?;
            let pending = self.pending_accept.take().unwrap();

            match pending.rx.recv() {
                Ok(result) => {
                    let _ = pending.handle.join();
                    let pipe = result?;
                    self.prime_next_accept()?;
                    Ok(pipe)
                }
                Err(_) => {
                    let _ = pending.handle.join();
                    Err(accept_thread_disconnected())
                }
            }
        }

        fn wait_for_pipe_timeout(&mut self, timeout: Duration) -> Result<File> {
            self.ensure_pending_accept()?;
            let pending = self.pending_accept.take().unwrap();

            match pending.rx.recv_timeout(timeout) {
                Ok(result) => {
                    let _ = pending.handle.join();
                    let pipe = result?;
                    self.prime_next_accept()?;
                    Ok(pipe)
                }
                Err(RecvTimeoutError::Timeout) => {
                    // Keep the same waiter alive; abandoning it would leave a stale
                    // server instance that clients can connect to without a handshake.
                    self.pending_accept = Some(pending);
                    Err(Error::TimedOut)
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = pending.handle.join();
                    Err(accept_thread_disconnected())
                }
            }
        }

        fn cancel_pending_accept(&mut self) {
            let Some(pending) = self.pending_accept.take() else {
                return;
            };

            if !pending.handle.is_finished() {
                // Best-effort: connect to our own pending instance so ConnectNamedPipe
                // returns and the waiter thread can exit before we drop the listener.
                let _ = connect_pipe(&self.pipe_path, Duration::from_millis(50));
                let _ = pending.rx.recv_timeout(Duration::from_millis(50));
            }

            if pending.handle.is_finished() {
                let _ = pending.handle.join();
            }
        }

        pub fn cleanup(_name: &str) {
            // Windows Named Pipe はハンドルを閉じれば自動クリーンアップ
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            self.cancel_pending_accept();
        }
    }

    pub fn connect_to(name: &str, config: ChannelConfig) -> Result<ShmemConnection> {
        let pipe_path = pipe_name(name);
        let mut pipe = connect_pipe(&pipe_path, config.connect_timeout)?;
        let conn_name = recv_conn_name(&mut pipe)?;
        let channel = Channel::open_with_config(&conn_name, config)?;
        Ok(channel.into_connection())
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
