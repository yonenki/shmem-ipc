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

/// 指定した PID のプロセスが生きているかチェックする
#[allow(dead_code)] // heartbeat 機能で使用予定
///
/// kill(pid, 0) はシグナルを送らずに存在確認だけ行う。
/// プロセスが存在すれば 0 を返し、存在しなければ ESRCH で -1 を返す。
pub fn is_process_alive(pid: u64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
