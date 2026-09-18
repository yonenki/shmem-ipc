use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::AtomicU32;
use std::time::Duration;

#[derive(Clone, Default)]
pub struct WaitHandle;

#[derive(Clone, Default)]
pub struct ChannelWaitSet {
    pub ring_a_writer: WaitHandle,
    pub ring_a_reader: WaitHandle,
    pub ring_b_writer: WaitHandle,
    pub ring_b_reader: WaitHandle,
}

impl ChannelWaitSet {
    pub fn new(_channel_name: &str, _wait_key: u64) -> std::io::Result<Self> {
        Ok(Self::default())
    }
}

/// futex_wait: *word == expected なら待機する (カーネルにスレッドを寝かせてもらう)
///
/// FUTEX_PRIVATE_FLAG を使わない。プロセス間共有メモリでは PRIVATE_FLAG があると
/// カーネルがプロセスローカルなハッシュテーブルで管理し、別プロセスの wake が届かない。
///
/// 戻り値は無視する:
/// - EAGAIN (値が変わった): 呼び出し元が condition を再チェックする
/// - ETIMEDOUT: 呼び出し元が deadline を確認する
/// - EINTR (シグナル割り込み): 再チェックでよい
fn futex_wait(word: &AtomicU32, expected: u32, timeout: Option<Duration>) {
    let ts = timeout.map(|d| libc::timespec {
        tv_sec: d.as_secs() as _,
        tv_nsec: d.subsec_nanos() as _,
    });
    let ts_ptr = ts
        .as_ref()
        .map_or(std::ptr::null(), |t| t as *const libc::timespec);

    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32 as *const u32,
            libc::FUTEX_WAIT, // PRIVATE_FLAG なし
            expected,
            ts_ptr,
            std::ptr::null::<u32>(), // uaddr2 (未使用)
            0u32,                    // val3 (未使用)
        );
    }
}

/// futex_wake: 待機中のスレッド/プロセスを1つ起こす
///
/// SPSC なので最大1つの waiter しかいない。
fn futex_wake(word: &AtomicU32) {
    unsafe {
        libc::syscall(
            libc::SYS_futex,
            word as *const AtomicU32 as *const u32,
            libc::FUTEX_WAKE, // PRIVATE_FLAG なし
            1i32,             // 1 waiter を起こす
            std::ptr::null::<libc::timespec>(),
            std::ptr::null::<u32>(),
            0u32,
        );
    }
}

/// 現在のプロセス ID を返す
pub fn wait_on_handle(
    _handle: &WaitHandle,
    word: &AtomicU32,
    expected: u32,
    timeout: Option<Duration>,
) {
    futex_wait(word, expected, timeout);
}

pub fn wake_handle(_handle: &WaitHandle, word: &AtomicU32) {
    futex_wake(word);
}

pub fn current_pid() -> u64 {
    unsafe { libc::getpid() as u64 }
}

/// A stable kernel process identity acquired from the bootstrap socket, not
/// from the shared header's asynchronously published client_pid.
pub(crate) struct PeerProcess(OwnedFd);

impl PeerProcess {
    pub(crate) fn from_socket(socket: &impl AsRawFd) -> crate::error::Result<Self> {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if len as usize != std::mem::size_of::<libc::ucred>() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "invalid peer credentials").into(),
            );
        }
        let pid = unsafe { credentials.assume_init() }.pid;
        if pid <= 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "peer PID unavailable").into());
        }
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
        if fd < 0 {
            let err = io::Error::last_os_error();
            return Err(if err.raw_os_error() == Some(libc::ESRCH) {
                crate::error::Error::PeerDisconnected
            } else {
                // ENOSYS, EPERM and resource exhaustion are setup failures;
                // never silently establish a connection without observation.
                err.into()
            });
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd as _) }))
    }

    pub(crate) fn check_ready(&self) -> crate::error::Result<()> {
        let mut fd = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let rc = unsafe { libc::poll(&mut fd, 1, 0) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            if fd.revents & libc::POLLNVAL != 0 {
                return Err(io::Error::from_raw_os_error(libc::EBADF).into());
            }
            if fd.revents & libc::POLLERR != 0 {
                return Err(io::Error::from_raw_os_error(libc::EIO).into());
            }
            if fd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Err(crate::error::Error::PeerDisconnected);
            }
            return Ok(());
        }
    }

    pub(crate) fn wait(&self, cancel: &PeerCancellation) -> io::Result<super::peer::PeerWait> {
        let mut fds = [
            libc::pollfd {
                fd: cancel.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
            if rc < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err);
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLNVAL != 0) {
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
            if fds.iter().any(|fd| fd.revents & libc::POLLERR != 0) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
            if fds[0].revents & libc::POLLIN != 0 {
                return Ok(super::peer::PeerWait::Cancelled);
            }
            if fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(super::peer::PeerWait::Exited);
            }
        }
    }
}

pub(crate) struct PeerCancellation(OwnedFd);

impl PeerCancellation {
    pub(crate) fn new() -> io::Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    pub(crate) fn cancel(&self) -> io::Result<()> {
        let value = 1u64;
        loop {
            let rc = unsafe {
                libc::write(
                    self.0.as_raw_fd(),
                    (&value as *const u64).cast(),
                    std::mem::size_of::<u64>(),
                )
            };
            if rc >= 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => return Err(err),
            }
        }
    }
}
