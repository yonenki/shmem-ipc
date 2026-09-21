#![cfg(any(windows, target_os = "linux"))]

use std::io::{BufRead, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, Error, Role, SpinThenWait};

fn config() -> ChannelConfig {
    ChannelConfig {
        ring_size: 256,
        wait_strategy: SpinThenWait { spin_count: 0 },
        connect_timeout: Duration::from_secs(5),
    }
}

fn unique_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!(
        "liveness_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

struct Peer {
    child: Child,
    name: String,
}

impl Peer {
    fn spawn(name: &str, role: Role) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_process", "--ignored", "--nocapture"])
            .env("SHMEM_LIVENESS_NAME", name)
            .env(
                "SHMEM_LIVENESS_ROLE",
                if role == Role::Server {
                    "server"
                } else {
                    "client"
                },
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            name: name.to_string(),
        }
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn close(&mut self) {
        let stdin = self.child.stdin.as_mut().unwrap();
        stdin.write_all(b"close\n").unwrap();
        stdin.flush().unwrap();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = Channel::cleanup(&self.name);
    }
}

/// A real second process, not a thread pretending to be a dead PID. The parent
/// exercises the channel before issuing a graceful close or Child::kill().
#[test]
#[ignore = "child process entry, launched by the liveness regressions"]
fn peer_process() {
    let name = std::env::var("SHMEM_LIVENESS_NAME").unwrap();
    let mut channel = if std::env::var("SHMEM_LIVENESS_ROLE").unwrap() == "server" {
        Channel::create_with_config(&name, config()).unwrap()
    } else {
        Channel::open_with_config(&name, config()).unwrap()
    };
    assert_eq!(channel.recv().unwrap(), b"hello");
    channel.send(b"ready").unwrap();
    for line in std::io::stdin().lock().lines() {
        match line.unwrap().as_str() {
            "close" => channel.close().unwrap(),
            command => panic!("unexpected child command: {command}"),
        }
    }
}

// Peer is bound before Channel so ordinary local drop order releases the local
// mapping before the test process/file cleanup guard.
fn pair(local_role: Role) -> (Peer, Channel) {
    let name = unique_name();
    let (peer, mut local) = match local_role {
        Role::Server => {
            let local = Channel::create_with_config(&name, config()).unwrap();
            (Peer::spawn(&name, Role::Client), local)
        }
        Role::Client => {
            let peer = Peer::spawn(&name, Role::Server);
            let local = Channel::open_with_config(&name, config()).unwrap();
            (peer, local)
        }
    };
    local.send(b"hello").unwrap();
    assert_eq!(local.recv().unwrap(), b"ready");
    (peer, local)
}

#[derive(Clone, Copy)]
enum Termination {
    Close,
    Kill,
}

impl Termination {
    fn trigger(self, peer: &mut Peer) {
        match self {
            Self::Close => peer.close(),
            Self::Kill => peer.kill(),
        }
    }

    fn assert_error<T: std::fmt::Debug>(self, result: shmem_ipc::Result<T>) {
        match self {
            Self::Close => assert!(matches!(result, Err(Error::ChannelClosed)), "{result:?}"),
            Self::Kill => assert!(matches!(result, Err(Error::PeerDisconnected)), "{result:?}"),
        }
    }
}

fn sync_split_waits(local_role: Role, termination: Termination) {
    let (mut peer, local) = pair(local_role);
    let (mut send, mut recv) = local.into_connection().split();
    send.send(&[7; 248]).unwrap(); // exactly fill the ring; the child never drains it
    let (started_tx, started_rx) = mpsc::channel();
    let (send_tx, send_rx) = mpsc::channel();
    let (recv_tx, recv_rx) = mpsc::channel();
    let recv_started = started_tx.clone();
    let send_thread = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        let result = send.send(b"must not publish");
        send_tx.send((send, result)).unwrap();
    });
    let recv_thread = std::thread::spawn(move || {
        recv_started.send(()).unwrap();
        let result = recv.recv();
        recv_tx.send((recv, result)).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let start = Instant::now();
    termination.trigger(&mut peer);
    let (mut send, send_result) = send_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let (mut recv, recv_result) = recv_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    termination.assert_error(send_result);
    termination.assert_error(recv_result);
    termination.assert_error(send.send(b"still disconnected"));
    termination.assert_error(recv.try_recv());
    match termination {
        Termination::Close => {
            // The remote process is still alive: close is not misclassified as death.
            assert!(peer.child.try_wait().unwrap().is_none());
            send.close().unwrap();
            recv.close().unwrap();
        }
        Termination::Kill => {
            termination.assert_error(send.close());
            termination.assert_error(recv.close());
        }
    }
    send_thread.join().unwrap();
    recv_thread.join().unwrap();
}

#[test]
fn sync_split_server_death_releases_empty_recv_and_full_send() {
    sync_split_waits(Role::Client, Termination::Kill);
}

#[test]
fn sync_split_client_death_releases_empty_recv_and_full_send() {
    sync_split_waits(Role::Server, Termination::Kill);
}

#[test]
fn sync_split_server_close_remains_normal_close() {
    sync_split_waits(Role::Client, Termination::Close);
}

#[test]
fn sync_split_client_close_remains_normal_close() {
    sync_split_waits(Role::Server, Termination::Close);
}

#[test]
fn sync_channel_detects_peer_death_without_split() {
    let (mut peer, mut local) = pair(Role::Client);
    peer.kill();
    let start = Instant::now();
    Termination::Kill.assert_error(local.recv_timeout(Duration::from_secs(2)));
    assert!(start.elapsed() < Duration::from_secs(1));
    Termination::Kill.assert_error(local.send(b"dead"));
    Termination::Kill.assert_error(local.try_recv());
    Termination::Kill.assert_error(local.close());
}

#[cfg(feature = "tokio")]
mod async_cases {
    use super::*;
    use shmem_ipc::tokio as ipc;

    async fn pair(local_role: Role) -> (Peer, ipc::Channel) {
        let name = unique_name();
        let (peer, mut local) = match local_role {
            Role::Server => {
                let local = ipc::Channel::create_with_config(&name, config())
                    .await
                    .unwrap();
                (Peer::spawn(&name, Role::Client), local)
            }
            Role::Client => {
                let peer = Peer::spawn(&name, Role::Server);
                let local = ipc::Channel::open_with_config(&name, config())
                    .await
                    .unwrap();
                (peer, local)
            }
        };
        local.send(b"hello").await.unwrap();
        assert_eq!(local.recv().await.unwrap(), b"ready");
        (peer, local)
    }

    async fn split_waits(local_role: Role, termination: Termination) {
        let (mut peer, local) = pair(local_role).await;
        let (mut send, mut recv) = local.into_connection().split();
        send.send(&[7; 248]).await.unwrap();
        let send_task = tokio::spawn(async move {
            let result = send.send(b"must not publish").await;
            (send, result)
        });
        let recv_task = tokio::spawn(async move {
            let result = recv.recv().await;
            (recv, result)
        });
        // On this current-thread runtime, both tasks poll into their real wait.
        tokio::task::yield_now().await;
        assert!(!send_task.is_finished());
        assert!(!recv_task.is_finished());
        let start = Instant::now();
        termination.trigger(&mut peer);
        let (send_join, recv_join) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(send_task, recv_task)
        })
        .await
        .expect("both wait directions must observe peer termination within one second");
        let (mut send, send_result) = send_join.unwrap();
        let (mut recv, recv_result) = recv_join.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        termination.assert_error(send_result);
        termination.assert_error(recv_result);
        termination.assert_error(send.send(b"still disconnected").await);
        termination.assert_error(recv.try_recv());
        termination.assert_error(recv.drain_ready(&mut Vec::new(), 4));
        match termination {
            Termination::Close => {
                assert!(peer.child.try_wait().unwrap().is_none());
                send.close().unwrap();
                recv.close().unwrap();
            }
            Termination::Kill => {
                termination.assert_error(send.close());
                termination.assert_error(recv.close());
            }
        }
    }

    #[tokio::test]
    async fn split_server_death_releases_empty_recv_and_full_send() {
        split_waits(Role::Client, Termination::Kill).await;
    }

    #[tokio::test]
    async fn split_client_death_releases_empty_recv_and_full_send() {
        split_waits(Role::Server, Termination::Kill).await;
    }

    #[tokio::test]
    async fn split_server_close_wakes_both_waiters() {
        split_waits(Role::Client, Termination::Close).await;
    }

    #[tokio::test]
    async fn split_client_close_wakes_both_waiters() {
        split_waits(Role::Server, Termination::Close).await;
    }

    #[tokio::test]
    async fn channel_and_connection_preserve_death_after_conversion() {
        let (mut peer, mut local) = pair(Role::Client).await;
        peer.kill();
        Termination::Kill.assert_error(
            tokio::time::timeout(Duration::from_secs(1), local.recv())
                .await
                .unwrap(),
        );
        Termination::Kill.assert_error(local.close());
        let mut connection = local.into_connection();
        Termination::Kill.assert_error(connection.send(b"dead").await);
        Termination::Kill.assert_error(connection.recv().await);
        Termination::Kill.assert_error(connection.close());
    }
}
