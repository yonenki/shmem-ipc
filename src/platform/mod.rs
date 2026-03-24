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
