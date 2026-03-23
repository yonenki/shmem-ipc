use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::platform;

/// ブロッキング戦略の trait
///
/// condition クロージャが `Ok(Some(value))` を返すまで待機する。
/// `Ok(None)` はまだ条件を満たさないことを意味する。
/// `Err(...)` は即座にエラーとして返される (ChannelClosed 検知など)。
pub trait WaitStrategy: Send + Sync {
    fn wait_until<T, F>(
        &self,
        notify: &AtomicU32,
        timeout: Option<Duration>,
        condition: F,
    ) -> Result<T>
    where
        F: Fn() -> Result<Option<T>>;
}

/// Spin → Futex フォールバック戦略 (製品デフォルト)
///
/// 1. spin_count 回だけ spin_loop() で回る (最低レイテンシ)
/// 2. それでもダメならカーネルの futex/WaitOnAddress に寝かせてもらう (CPU 節約)
///
/// futex_wait は notify ワードの値が変わっていなければスリープし、
/// 相手が futex_wake を呼ぶと起きる。
pub struct SpinThenWait {
    pub spin_count: u32,
}

impl Default for SpinThenWait {
    fn default() -> Self {
        Self { spin_count: 256 }
    }
}

impl WaitStrategy for SpinThenWait {
    fn wait_until<T, F>(
        &self,
        notify: &AtomicU32,
        timeout: Option<Duration>,
        condition: F,
    ) -> Result<T>
    where
        F: Fn() -> Result<Option<T>>,
    {
        // Phase 1: Spin
        for _ in 0..self.spin_count {
            match condition()? {
                Some(val) => return Ok(val),
                None => std::hint::spin_loop(),
            }
        }

        // Phase 2: Futex
        //
        // 重要: snapshot を condition チェックの *前* に取る。
        //
        // 誤った順序 (condition → snapshot → futex_wait):
        //   1. condition() → None (データまだない)
        //   2. 相手がデータ publish + notify bump + futex_wake
        //   3. snapshot = notify.load() → 新しい値 V+1
        //   4. futex_wait(notify, V+1) → 値が一致するのでスリープ
        //   5. 相手は応答待ちなので wake が来ない → デッドロック
        //
        // 正しい順序 (snapshot → condition → futex_wait):
        //   1. snapshot = notify.load() → 現在の値 V
        //   2. condition() → None
        //   3. 相手がデータ publish + notify bump (V → V+1) + futex_wake
        //   4. futex_wait(notify, V) → *notify(=V+1) != V なので即リターン (EAGAIN)
        //   5. ループで condition() を再チェック → データあり → 成功
        let deadline = timeout.map(|d| Instant::now() + d);
        loop {
            // snapshot を先に取る
            let snapshot = notify.load(Ordering::Acquire);

            // condition チェック
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

            // snapshot 時点の値で futex_wait。
            // もし snapshot 後に notify が bump されていれば、
            // futex_wait は即座に EAGAIN でリターンする。
            platform::futex_wait(notify, snapshot, remaining);
        }
    }
}

/// 純 Spin 戦略 (ベンチマーク用)
///
/// ipc-bench 互換。CPU を 100% 使うが最低レイテンシ。
/// タイムアウトチェックは 1024 イテレーションごと (clock_gettime のコスト回避)。
pub struct SpinOnly;

impl WaitStrategy for SpinOnly {
    fn wait_until<T, F>(
        &self,
        _notify: &AtomicU32,
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
                // 1024 回ごとに時刻確認 (毎回だと clock_gettime のコストが支配的になる)
                if iter & 0x3FF == 0 && Instant::now() >= dl {
                    return Err(Error::TimedOut);
                }
            }
        }
    }
}
