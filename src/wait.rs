use std::sync::atomic::{AtomicU32, Ordering, fence};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::platform;

/// ブロッキング戦略の trait
pub trait WaitStrategy: Send + Sync {
    /// condition が Ok(Some(val)) を返すまで待機する。
    /// notify: 相手が bump する futex ワード
    /// parked: 自分が futex に入っていることを相手に伝えるフラグ
    fn wait_until<T, F>(
        &self,
        handle: &platform::WaitHandle,
        notify: &AtomicU32,
        parked: &AtomicU32,
        timeout: Option<Duration>,
        condition: F,
    ) -> Result<T>
    where
        F: Fn() -> Result<Option<T>>;

    /// 相手が parked していれば futex_wake を呼ぶ。していなければ何もしない。
    fn wake_if_parked(
        &self,
        handle: &platform::WaitHandle,
        notify: &AtomicU32,
        parked: &AtomicU32,
    ) {
        // notify は既に bump 済みの前提で呼ばれる
        if parked.load(Ordering::Acquire) != 0 {
            platform::wake_handle(handle, notify);
        }
    }

    /// 強制 wake (close 時)。parked に関わらず呼ぶ。
    fn wake_force(&self, handle: &platform::WaitHandle, notify: &AtomicU32) {
        platform::wake_handle(handle, notify);
    }
}

/// Spin → Futex フォールバック戦略 (製品デフォルト)
#[derive(Clone)]
pub struct SpinThenWait {
    pub spin_count: u32,
}

impl Default for SpinThenWait {
    fn default() -> Self {
        Self { spin_count: 512 }
    }
}

impl WaitStrategy for SpinThenWait {
    fn wait_until<T, F>(
        &self,
        handle: &platform::WaitHandle,
        notify: &AtomicU32,
        parked: &AtomicU32,
        timeout: Option<Duration>,
        condition: F,
    ) -> Result<T>
    where
        F: Fn() -> Result<Option<T>>,
    {
        // Phase 1: Spin (syscall なし、最低レイテンシ)
        for _ in 0..self.spin_count {
            match condition()? {
                Some(val) => return Ok(val),
                None => std::hint::spin_loop(),
            }
        }

        // Phase 2: Futex (parked フラグで相手に通知)
        let deadline = timeout.map(|d| Instant::now() + d);
        loop {
            let snapshot = notify.load(Ordering::Acquire);

            match condition()? {
                Some(val) => return Ok(val),
                None => {}
            }

            let remaining = match deadline {
                Some(dl) => {
                    let now = Instant::now();
                    if now >= dl {
                        return Err(Error::TimedOut);
                    }
                    Some(dl - now)
                }
                None => None,
            };

            // parked = 1: 「自分は寝る」と宣言
            parked.store(1, Ordering::Release);

            // StoreLoad バリア: parked の store がグローバルに可視になってから
            // condition の load を実行することを保証する。
            // これがないと x86-64 の store buffer bypass により、waker 側の
            // parked.load() が 0 を読み、同時にこちらの condition() も古い値を読む
            // Dekker パターンの missed wakeup が発生し得る。
            // Linux では futex の atomic check-and-sleep がこの役割を担うが、
            // Windows の WaitForSingleObject にはその機構がないため必須。
            fence(Ordering::SeqCst);

            // 宣言後にもう一度チェック (parked セット前に相手が publish した場合を拾う)
            match condition()? {
                Some(val) => {
                    parked.store(0, Ordering::Relaxed);
                    return Ok(val);
                }
                None => {}
            }

            platform::wait_on_handle(handle, notify, snapshot, remaining);
            parked.store(0, Ordering::Relaxed);
        }
    }
}

/// 純 Spin 戦略 (ベンチマーク用)
pub struct SpinOnly;

impl WaitStrategy for SpinOnly {
    fn wait_until<T, F>(
        &self,
        _handle: &platform::WaitHandle,
        _notify: &AtomicU32,
        _parked: &AtomicU32,
        timeout: Option<Duration>,
        condition: F,
    ) -> Result<T>
    where
        F: Fn() -> Result<Option<T>>,
    {
        let deadline = timeout.map(|d| Instant::now() + d);
        let mut iter: u32 = 0;
        loop {
            match condition()? {
                Some(val) => return Ok(val),
                None => std::hint::spin_loop(),
            }
            iter = iter.wrapping_add(1);
            if let Some(dl) = deadline {
                if iter & 0x3FF == 0 && Instant::now() >= dl {
                    return Err(Error::TimedOut);
                }
            }
        }
    }
}
