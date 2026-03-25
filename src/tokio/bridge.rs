#[cfg(unix)]
mod imp {
    use std::io;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering, fence};
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use ::tokio::sync::Notify;

    use crate::error::{Error, Result};
    use crate::platform;
    use crate::ring::WaitTarget;

    pub(crate) struct AsyncWait {
        target: WaitTarget,
        running: Option<RunningWait>,
        spin_count: u32,
    }

    impl AsyncWait {
        pub(crate) fn new(target: WaitTarget, spin_count: u32) -> Self {
            Self {
                target,
                running: None,
                spin_count,
            }
        }

        pub(crate) fn snapshot(&mut self) -> Result<u64> {
            self.ensure_running()?;
            Ok(self.target.notify().load(Ordering::Acquire) as u64)
        }

        pub(crate) fn spin_count(&self) -> u32 {
            self.spin_count
        }

        pub(crate) async fn wait_changed(&mut self, observed: u64) -> Result<()> {
            self.ensure_running()?;
            let running = self.running.as_ref().expect("wait bridge missing");
            let observed = observed as u32;

            loop {
                let current = self.target.notify().load(Ordering::Acquire);
                if current != observed {
                    return Ok(());
                }

                let notified = running.state.notify.notified();
                let current = self.target.notify().load(Ordering::Acquire);
                if current != observed {
                    return Ok(());
                }

                notified.await;
            }
        }

        fn ensure_running(&mut self) -> Result<()> {
            if self.running.is_none() {
                self.running = Some(RunningWait::spawn(self.target.clone())?);
            }
            Ok(())
        }
    }

    impl Drop for AsyncWait {
        fn drop(&mut self) {
            if let Some(mut running) = self.running.take() {
                running.stop(&self.target);
            }
        }
    }

    struct RunningWait {
        state: Arc<WaitState>,
        thread: Option<JoinHandle<()>>,
    }

    impl RunningWait {
        fn spawn(target: WaitTarget) -> Result<Self> {
            let state = Arc::new(WaitState {
                stop: AtomicBool::new(false),
                notify: Notify::new(),
            });
            let thread_state = Arc::clone(&state);
            let thread_target = target.clone();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);

            let thread = std::thread::Builder::new()
                .name("shmem-ipc-async-wait".to_string())
                .spawn(move || wait_loop(thread_target, thread_state, ready_tx))
                .map_err(|err| {
                    Error::Io(io::Error::other(format!(
                        "failed to spawn async wait bridge: {err}"
                    )))
                })?;

            ready_rx.recv().map_err(|err| {
                Error::Io(io::Error::other(format!(
                    "async wait bridge failed to start: {err}"
                )))
            })?;

            Ok(Self {
                state,
                thread: Some(thread),
            })
        }

        fn stop(&mut self, target: &WaitTarget) {
            self.state.stop.store(true, Ordering::Release);
            target.notify().fetch_add(1, Ordering::Release);
            platform::wake_handle(target.handle(), target.notify());
            self.state.notify.notify_waiters();

            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct WaitState {
        stop: AtomicBool,
        notify: Notify,
    }

    fn wait_loop(target: WaitTarget, state: Arc<WaitState>, ready: mpsc::SyncSender<()>) {
        let notify = target.notify();
        let parked = target.parked();
        let _ = ready.send(());

        while !state.stop.load(Ordering::Acquire) {
            let observed = notify.load(Ordering::Acquire);
            parked.store(1, Ordering::Release);
            fence(Ordering::SeqCst);

            let current = notify.load(Ordering::Acquire);
            if current != observed {
                parked.store(0, Ordering::Relaxed);
                state.notify.notify_waiters();
                continue;
            }

            platform::wait_on_handle(target.handle(), notify, observed, None);
            parked.store(0, Ordering::Relaxed);
            let current = notify.load(Ordering::Acquire);

            if state.stop.load(Ordering::Acquire) {
                break;
            }

            if current != observed {
                state.notify.notify_waiters();
            }
        }
    }
}

#[cfg(windows)]
mod imp {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{Ordering, fence};
    use std::task::{Context, Poll};

    use crate::error::{Error, Result};
    use crate::platform;
    use crate::ring::WaitTarget;

    pub(crate) struct AsyncWait {
        target: WaitTarget,
        spin_count: u32,
    }

    impl AsyncWait {
        pub(crate) fn new(target: WaitTarget, spin_count: u32) -> Self {
            Self { target, spin_count }
        }

        pub(crate) fn snapshot(&mut self) -> Result<u64> {
            Ok(self.target.notify().load(Ordering::Acquire) as u64)
        }

        pub(crate) fn spin_count(&self) -> u32 {
            self.spin_count
        }

        pub(crate) async fn wait_changed(&mut self, observed: u64) -> Result<()> {
            WaitChanged {
                target: &self.target,
                observed: observed as u32,
                armed: false,
            }
            .await
        }
    }

    struct WaitChanged<'a> {
        target: &'a WaitTarget,
        observed: u32,
        armed: bool,
    }

    impl WaitChanged<'_> {
        fn disarm(&mut self) {
            if self.armed {
                platform::clear_wait_handle(self.target.handle());
                self.target.parked().store(0, Ordering::Relaxed);
                self.armed = false;
            }
        }
    }

    impl Drop for WaitChanged<'_> {
        fn drop(&mut self) {
            self.disarm();
        }
    }

    impl Future for WaitChanged<'_> {
        type Output = Result<()>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            loop {
                if self.target.notify().load(Ordering::Acquire) != self.observed {
                    self.disarm();
                    return Poll::Ready(Ok(()));
                }

                if !self.armed {
                    self.target.parked().store(1, Ordering::Release);
                    fence(Ordering::SeqCst);
                    platform::prepare_wait_handle(self.target.handle()).map_err(Error::Io)?;
                    self.armed = true;
                    continue;
                }

                match platform::poll_wait_handle(self.target.handle(), cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(())) => continue,
                    Poll::Ready(Err(err)) => {
                        self.disarm();
                        return Poll::Ready(Err(Error::Io(err)));
                    }
                }
            }
        }
    }
}

pub(crate) use imp::AsyncWait;
