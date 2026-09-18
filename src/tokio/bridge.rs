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

                // notify_waiters covers a Notified as soon as it is created,
                // even before its first poll. Create it before the final word
                // check so a handoff between that check and await is retained.
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
            Self::spawn_inner(
                target,
                #[cfg(test)]
                None,
            )
        }

        fn spawn_inner(
            target: WaitTarget,
            #[cfg(test)] hooks: Option<tests::WaitHooks>,
        ) -> Result<Self> {
            let state = Arc::new(WaitState {
                stop: AtomicBool::new(false),
                notify: Notify::new(),
            });
            let thread_state = Arc::clone(&state);
            let thread_target = target.clone();
            let (ready_tx, ready_rx) = mpsc::sync_channel(1);

            let thread = std::thread::Builder::new()
                .name("shmem-ipc-async-wait".to_string())
                .spawn(move || {
                    wait_loop(
                        thread_target,
                        thread_state,
                        ready_tx,
                        #[cfg(test)]
                        hooks,
                    )
                })
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

    fn wait_loop(
        target: WaitTarget,
        state: Arc<WaitState>,
        ready: mpsc::SyncSender<()>,
        #[cfg(test)] hooks: Option<tests::WaitHooks>,
    ) {
        let notify = target.notify();
        let parked = target.parked();
        // Readiness lets the application park. Keep this baseline until its
        // changes have been handed off, including across re-arming the futex.
        let mut observed = notify.load(Ordering::Acquire);
        let _ = ready.send(());

        while !state.stop.load(Ordering::Acquire) {
            #[cfg(test)]
            if let Some(hooks) = &hooks {
                hooks.before_park();
            }
            parked.store(1, Ordering::Release);
            fence(Ordering::SeqCst);

            let current = notify.load(Ordering::Acquire);
            // Stop may have been published before the last observed notification.
            // Recheck it before sleeping so final-owner join cannot miss it.
            if state.stop.load(Ordering::Acquire) {
                parked.store(0, Ordering::Relaxed);
                break;
            }
            if current != observed {
                parked.store(0, Ordering::Relaxed);
                observed = current;
                state.notify.notify_waiters();
                continue;
            }

            #[cfg(test)]
            if let Some(hooks) = &hooks {
                hooks.before_wait();
            }

            platform::wait_on_handle(target.handle(), notify, observed, None);
            parked.store(0, Ordering::Relaxed);
            let current = notify.load(Ordering::Acquire);

            if state.stop.load(Ordering::Acquire) {
                break;
            }

            if current != observed {
                observed = current;
                state.notify.notify_waiters();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};
        use std::pin::Pin;
        use std::process::{Child, Command, Stdio};
        use std::sync::atomic::AtomicU64;
        use std::task::{Context, Poll, Wake, Waker};
        use std::time::{Duration, Instant};

        use crate::{ChannelConfig, ShmemListener, SpinThenWait};

        const DEADLINE: Duration = Duration::from_secs(10);
        const FULL_RING: [u8; 248] = [0xAB; 248];

        #[derive(Debug, PartialEq, Eq)]
        enum BridgeEvent {
            BeforePark,
            BeforeWait,
        }

        pub(super) struct WaitHooks {
            events: mpsc::Sender<BridgeEvent>,
            resume: mpsc::Receiver<()>,
        }

        impl WaitHooks {
            pub(super) fn before_park(&self) {
                let _ = self.events.send(BridgeEvent::BeforePark);
                // Disconnection releases the bridge during assertion unwinding,
                // before AsyncWait's normal stop/wake/join tears it down.
                let _ = self.resume.recv();
            }

            pub(super) fn before_wait(&self) {
                let _ = self.events.send(BridgeEvent::BeforeWait);
            }
        }

        struct BridgeControl {
            events: mpsc::Receiver<BridgeEvent>,
            resume: mpsc::Sender<()>,
        }

        impl BridgeControl {
            fn expect(&self, event: BridgeEvent) {
                assert_eq!(self.events.recv_timeout(DEADLINE).unwrap(), event);
            }

            fn resume(&self) {
                self.resume.send(()).unwrap();
            }
        }

        #[derive(Clone, Copy)]
        enum Direction {
            Recv,
            Send,
        }

        #[derive(Clone, Copy)]
        enum Gap {
            Startup,
            Rearm,
        }

        fn config() -> ChannelConfig {
            ChannelConfig {
                ring_size: 256,
                wait_strategy: SpinThenWait { spin_count: 0 },
                ..ChannelConfig::default()
            }
        }

        struct Peer(Child);

        impl Peer {
            fn spawn(name: &str) -> Self {
                Self(
                    Command::new(std::env::current_exe().unwrap())
                        .args([
                            "--exact",
                            "tokio::bridge::imp::tests::bridge_peer_child",
                            "--nocapture",
                            "--test-threads=1",
                        ])
                        .env("SHMEM_BRIDGE_TEST_NAME", name)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .spawn()
                        .unwrap(),
                )
            }

            fn advance(&mut self, direction: Direction) {
                let command = match direction {
                    Direction::Recv => b'S',
                    Direction::Send => b'R',
                };
                let input = self.0.stdin.as_mut().unwrap();
                input.write_all(&[command]).unwrap();
                input.flush().unwrap();
            }

            fn kill(&mut self) {
                self.0.kill().unwrap();
                self.0.wait().unwrap();
            }
        }

        impl Drop for Peer {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn bridge_peer_child() {
            let Ok(name) = std::env::var("SHMEM_BRIDGE_TEST_NAME") else {
                return;
            };
            let mut connection = crate::connect(&name, config()).unwrap();
            let mut input = std::io::stdin().lock();
            loop {
                let mut command = [0];
                input.read_exact(&mut command).unwrap();
                match command[0] {
                    b'S' => connection.send(b"warmup").unwrap(),
                    b'R' => assert_eq!(connection.recv().unwrap(), FULL_RING),
                    _ => panic!("unexpected bridge fixture command"),
                }
            }
        }

        fn install_gate(
            connection: &mut crate::tokio::ShmemConnection,
            direction: Direction,
        ) -> (WaitTarget, BridgeControl) {
            let wait = match direction {
                Direction::Recv => connection.recv_wait.as_mut().unwrap(),
                Direction::Send => connection.send_wait.as_mut().unwrap(),
            };
            let target = wait.target.clone();
            let (events_tx, events) = mpsc::channel();
            let (resume, resume_rx) = mpsc::channel();
            wait.running = Some(
                RunningWait::spawn_inner(
                    target.clone(),
                    Some(WaitHooks {
                        events: events_tx,
                        resume: resume_rx,
                    }),
                )
                .unwrap(),
            );
            (target, BridgeControl { events, resume })
        }

        struct TaskWake(mpsc::Sender<()>);

        impl Wake for TaskWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                let _ = self.0.send(());
            }
        }

        fn task_waker() -> (Waker, mpsc::Receiver<()>) {
            let (wake, wakes) = mpsc::channel();
            (Waker::from(Arc::new(TaskWake(wake))), wakes)
        }

        fn assert_pending(future: Pin<&mut impl Future>, waker: &Waker) {
            assert!(future.poll(&mut Context::from_waker(waker)).is_pending());
        }

        fn finish_on_wake<F: Future>(
            future: Pin<&mut F>,
            waker: &Waker,
            wakes: &mpsc::Receiver<()>,
        ) -> F::Output {
            // Never repoll on a timer: doing so could discover the latched
            // terminal error without a bridge wake and conceal the regression.
            wakes
                .recv_timeout(DEADLINE)
                .expect("bridge did not wake the pending operation");
            match future.poll(&mut Context::from_waker(waker)) {
                Poll::Ready(result) => result,
                Poll::Pending => panic!("operation remained pending after bridge handoff"),
            }
        }

        async fn operation(
            connection: &mut crate::tokio::ShmemConnection,
            direction: Direction,
        ) -> Result<Vec<u8>> {
            match direction {
                Direction::Recv => connection.recv().await,
                Direction::Send => {
                    connection.send(&FULL_RING).await?;
                    Ok(Vec::new())
                }
            }
        }

        fn wait_for_terminal_notification(target: &WaitTarget, observed: u32) {
            let deadline = Instant::now() + DEADLINE;
            while target.notify().load(Ordering::Acquire) == observed {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "native peer monitor did not notify");
                // The bridge is held at BeforePark, so this is the sole native
                // waiter. Observe PeerWake's real increment/wake, never inject
                // a terminal state or produce a second notification ourselves.
                platform::wait_on_handle(
                    target.handle(),
                    target.notify(),
                    observed,
                    Some(remaining),
                );
            }
        }

        fn peer_death_at_gap(direction: Direction, gap: Gap) {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let name = format!(
                "bridge_{}_{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let mut listener = ShmemListener::bind(&name, config()).unwrap();
            let mut peer = Peer::spawn(&name);
            let mut connection = listener.accept_timeout(DEADLINE).unwrap();
            if matches!(direction, Direction::Send) {
                connection.send(&FULL_RING).unwrap();
            }
            let mut connection = crate::tokio::ShmemConnection::from_sync(connection);
            // Drop the controller before the connection, also on assertion
            // failure, to release any paused bridge before stop/join.
            let (target, bridge) = install_gate(&mut connection, direction);
            bridge.expect(BridgeEvent::BeforePark);

            if matches!(gap, Gap::Rearm) {
                let (waker, wakes) = task_waker();
                let mut pending = std::pin::pin!(operation(&mut connection, direction));
                assert_pending(pending.as_mut(), &waker);
                bridge.resume();
                // The initial baseline and final word check are complete
                // before the warmup. Only the subsequent re-arm is held.
                bridge.expect(BridgeEvent::BeforeWait);
                peer.advance(direction);
                let result = finish_on_wake(pending.as_mut(), &waker, &wakes).unwrap();
                if matches!(direction, Direction::Recv) {
                    assert_eq!(result, b"warmup");
                }
                bridge.expect(BridgeEvent::BeforePark);
            }

            {
                let (waker, wakes) = task_waker();
                let mut pending = std::pin::pin!(operation(&mut connection, direction));
                assert_pending(pending.as_mut(), &waker);
                let observed = target.notify().load(Ordering::Acquire);

                peer.kill();
                wait_for_terminal_notification(&target, observed);
                bridge.resume();

                let result = finish_on_wake(pending.as_mut(), &waker, &wakes);
                assert!(
                    matches!(result, Err(Error::PeerDisconnected)),
                    "expected native peer death, got {result:?}"
                );
                assert_eq!(
                    target.notify().load(Ordering::Acquire),
                    observed.wrapping_add(1),
                    "completion must not need another notification"
                );
            }
        }

        #[test]
        fn peer_death_before_first_recv_park() {
            peer_death_at_gap(Direction::Recv, Gap::Startup);
        }

        #[test]
        fn peer_death_before_recv_rearm() {
            peer_death_at_gap(Direction::Recv, Gap::Rearm);
        }

        #[test]
        fn peer_death_before_first_send_park() {
            peer_death_at_gap(Direction::Send, Gap::Startup);
        }

        #[test]
        fn peer_death_before_send_rearm() {
            peer_death_at_gap(Direction::Send, Gap::Rearm);
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
