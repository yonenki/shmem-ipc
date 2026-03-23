use std::path::PathBuf;

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
