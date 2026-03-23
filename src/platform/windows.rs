use std::sync::atomic::AtomicU32;
use std::time::Duration;

/// Windows でのクロスプロセス待機
///
/// WaitOnAddress / WakeByAddressSingle はプロセス内でのみ動作する
/// (Raymond Chen: "WaitOnAddress works only within a process")。
/// クロスプロセス共有メモリでは使えないため、spin + 短い sleep にフォールバックする。
///
/// 将来的には Named Event (CreateEventW) でプロセス間通知を実装可能だが、
/// 現時点では spin + sleep で安全に動作する実装とする。
/// notify ワードはデータの到着を示すフラグとして引き続き使い、
/// spin ループ内で Acquire load して変化を検出する。
pub fn futex_wait(_word: &AtomicU32, _expected: u32, timeout: Option<Duration>) {
    // WaitOnAddress はクロスプロセスで動作しないため、短い sleep で代替。
    // SpinThenWait の spin フェーズで大半のケースは即座にデータを検出でき、
    // ここに来るのは相手が遅い場合のみ。
    let sleep_duration = timeout
        .map(|d| d.min(Duration::from_micros(100)))
        .unwrap_or(Duration::from_micros(100));
    std::thread::sleep(sleep_duration);
}

/// Windows でのクロスプロセス wake
///
/// WakeByAddressSingle はプロセス内限定のため、クロスプロセスでは no-op。
/// 相手は spin または sleep から自然に復帰し、notify ワードの変化で検出する。
pub fn futex_wake(_word: &AtomicU32) {
    // クロスプロセスでは no-op。
    // notify ワードの Acquire load による spin で検出される。
}

/// 現在のプロセス ID を返す
pub fn current_pid() -> u64 {
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    unsafe { GetCurrentProcessId() as u64 }
}

/// 指定した PID のプロセスが生きているかチェックする
#[allow(dead_code)]
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
