use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::*;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use unsupported::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub pid: u64,
    pub birth: u64,
}

pub(crate) fn listener_incarnation() -> crate::Result<String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let identity = current_identity()?;
    Ok(format!(
        "{}.{:x}.{:x}",
        identity.pid,
        identity.birth,
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// 共有メモリファイルのパスを生成する
pub fn shm_path(name: &str) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        // /dev/shm/ は tmpfs (RAM上) — ディスク I/O なし
        PathBuf::from(format!("/dev/shm/shmem_ipc_{name}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::temp_dir().join(format!("shmem_ipc_{name}"))
    }
}

pub fn new_wait_key() -> u64 {
    static NEXT_WAIT_KEY: AtomicU64 = AtomicU64::new(1);

    let counter = NEXT_WAIT_KEY.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    now.rotate_left(17) ^ current_pid().wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ counter
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn live_pid_with_a_different_birth_is_disconnected() {
        let identity = current_identity().unwrap();
        assert!(ProcessMonitor::open(identity).unwrap().is_alive().unwrap());
        let wrong_birth = ProcessIdentity {
            birth: identity.birth ^ 1,
            ..identity
        };
        assert!(matches!(
            ProcessMonitor::open(wrong_birth),
            Err(crate::Error::PeerDisconnected)
        ));
    }
}
