use std::sync::atomic::AtomicU32;
use std::time::Duration;

/// WaitOnAddress: *word == expected なら待機する (Windows 版 futex)
///
/// Windows 8+ で利用可能。内部的にカーネルが物理アドレスで管理するので
/// mmap 共有メモリ上の AtomicU32 に対しても正しく動作する。
pub fn futex_wait(word: &AtomicU32, expected: u32, timeout: Option<Duration>) {
    use windows_sys::Win32::System::Threading::WaitOnAddress;

    let timeout_ms = timeout
        .map(|d| d.as_millis().min(u32::MAX as u128) as u32)
        .unwrap_or(u32::MAX); // INFINITE

    unsafe {
        WaitOnAddress(
            word as *const AtomicU32 as *const std::ffi::c_void,
            &expected as *const u32 as *const std::ffi::c_void,
            size_of::<u32>(),
            timeout_ms,
        );
    }
}

/// WakeByAddressSingle: 待機中のスレッド/プロセスを1つ起こす
pub fn futex_wake(word: &AtomicU32) {
    use windows_sys::Win32::System::Threading::WakeByAddressSingle;

    unsafe {
        WakeByAddressSingle(word as *const AtomicU32 as *const std::ffi::c_void);
    }
}

/// 現在のプロセス ID を返す
pub fn current_pid() -> u64 {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    unsafe { GetCurrentProcessId() as u64 }
}

/// 指定した PID のプロセスが生きているかチェックする
pub fn is_process_alive(pid: u64) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess};

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}
