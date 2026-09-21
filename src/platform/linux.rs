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

/// /proc starttime is field 22, in clock ticks since boot (not milliseconds).
fn process_stat(pid: u64) -> crate::Result<(u64, bool)> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(crate::Error::PeerDisconnected);
        }
        Err(error) => return Err(error.into()),
    };
    parse_process_stat(&stat)
}

fn parse_process_stat(stat: &str) -> crate::Result<(u64, bool)> {
    // comm may contain spaces and ')' characters.
    let (_, fields) = stat.rsplit_once(") ").ok_or(crate::Error::InvalidHeader)?;
    let mut fields = fields.split_whitespace();
    let state = fields.next().ok_or(crate::Error::InvalidHeader)?;
    let birth = fields
        .nth(18)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or(crate::Error::InvalidHeader)?;
    Ok((birth, !matches!(state, "Z" | "X" | "x")))
}

pub struct ProcessMonitor {
    identity: super::ProcessIdentity,
}

impl ProcessMonitor {
    pub fn open(identity: super::ProcessIdentity) -> crate::Result<Self> {
        if identity.pid == 0 {
            return Err(crate::Error::InvalidHeader);
        }
        let monitor = Self { identity };
        if !monitor.is_alive()? {
            return Err(crate::Error::PeerDisconnected);
        }
        Ok(monitor)
    }

    pub fn is_alive(&self) -> crate::Result<bool> {
        match process_stat(self.identity.pid) {
            Ok((birth, alive)) => Ok(alive && birth == self.identity.birth),
            Err(crate::Error::PeerDisconnected) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

pub fn current_identity() -> crate::Result<super::ProcessIdentity> {
    let pid = current_pid();
    let (birth, _) = process_stat(pid).map_err(|error| match error {
        crate::Error::PeerDisconnected => crate::Error::UnsupportedPlatform,
        other => other,
    })?;
    Ok(super::ProcessIdentity { pid, birth })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_birth_uses_starttime_ticks_and_recognizes_zombies() {
        let fields = "1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987654321 99";
        assert_eq!(
            parse_process_stat(&format!("42 (worker (name)) S {fields}")).unwrap(),
            (987654321, true)
        );
        assert_eq!(
            parse_process_stat(&format!("42 (worker) Z {fields}")).unwrap(),
            (987654321, false)
        );
    }
}
