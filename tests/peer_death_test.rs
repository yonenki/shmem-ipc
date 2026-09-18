//! The controller never calls a potentially blocking IPC operation. Each
//! endpoint runs in a real child; an external deadline kills and reaps only
//! fixture-owned children even if a sync wait, Tokio executor or Drop hangs.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, Error, ShmemConnection, ShmemListener};

const DEADLINE: Duration = Duration::from_secs(10);
const FULL_RING: [u8; 248] = [0xAB; 248];
const QUEUED: [&[u8]; 3] = [b"one", b"two", b"three"];

fn config() -> ChannelConfig {
    ChannelConfig {
        ring_size: 256,
        wait_strategy: shmem_ipc::SpinThenWait { spin_count: 0 },
        ..ChannelConfig::default()
    }
}

struct EndpointName(String);

impl EndpointName {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(format!(
            "peer_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
}

impl Drop for EndpointName {
    fn drop(&mut self) {
        ShmemListener::cleanup(&self.0);
        for generation in 0..4 {
            let _ = Channel::cleanup(&format!("{}.conn.{generation}", self.0));
        }
        let _ = Channel::cleanup(&format!("{}.raw", self.0));
    }
}

struct Actor {
    child: Child,
    input: ChildStdin,
    events: Receiver<String>,
}

impl Actor {
    fn spawn(name: &str, role: &str, api: &str, action: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "peer_child", "--nocapture", "--test-threads=1"])
            .env("SHMEM_PEER_TEST_NAME", name)
            .env("SHMEM_PEER_TEST_ROLE", role)
            .env("SHMEM_PEER_TEST_API", api)
            .env("SHMEM_PEER_TEST_ACTION", action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let line = line.unwrap();
                if let Some((_, event)) = line.split_once("PEER_EVENT ") {
                    if tx.send(event.to_owned()).is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            child,
            input,
            events,
        }
    }

    fn send(&mut self, command: &str) {
        writeln!(self.input, "{command}").unwrap();
        self.input.flush().unwrap();
    }

    fn expect(&self, expected: &str) {
        let actual = self.events.recv_timeout(DEADLINE).unwrap_or_else(|err| {
            panic!("child {} waiting for {expected}: {err}", self.child.id())
        });
        assert_eq!(actual, expected, "child {}", self.child.id());
    }

    fn remains_pending(&self) {
        assert!(
            matches!(
                self.events.recv_timeout(Duration::from_millis(100)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "operation completed before peer exit"
        );
    }

    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    fn finish(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "child failed: {status}");
                return;
            }
            assert!(Instant::now() < deadline, "child teardown hung");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        // Also runs on controller assertion failure, before EndpointName cleanup.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pair(
    name: &str,
    api: &str,
    survivor_role: &str,
    survivor_action: &str,
    peer_action: &str,
) -> (Actor, Actor) {
    let server_action = if survivor_role == "server" {
        survivor_action
    } else {
        peer_action
    };
    let client_action = if survivor_role == "client" {
        survivor_action
    } else {
        peer_action
    };
    let server = Actor::spawn(name, "server", api, server_action);
    server.expect("listening");
    let client = Actor::spawn(name, "client", api, client_action);
    server.expect("connected");
    client.expect("connected");
    if survivor_role == "server" {
        (server, client)
    } else {
        (client, server)
    }
}

#[test]
fn native_peer_lifecycle() {
    let apis = [
        "sync",
        #[cfg(feature = "tokio")]
        "tokio",
    ];

    for api in apis {
        // Both endpoint identities and both parked operation directions. The
        // peer sends no application traffic to wake an idle receive.
        for role in ["server", "client"] {
            for operation in ["recv", "send", "split", "split-send"] {
                let name = EndpointName::new();
                let (mut survivor, mut peer) = pair(&name.0, api, role, operation, "source");
                survivor.expect("waiting");
                survivor.remains_pending();
                peer.kill();
                survivor.expect("terminal");
                survivor.finish();
            }
        }

        // Observe native death via the send side *before* draining, so the
        // partial-success batch branch is exercised with the cause latched.
        let name = EndpointName::new();
        let (mut survivor, mut peer) = pair(&name.0, api, "server", "drain", "source");
        peer.send("queue");
        peer.expect("queued");
        peer.send("crash"); // exit() skips Rust destructors, unlike graceful Drop.
        peer.finish();
        survivor.send("drain");
        survivor.expect("drained");
        survivor.finish();

        // A graceful final-owner drop terminates IPC while the process stays
        // alive, and remains ChannelClosed after that process subsequently dies.
        let name = EndpointName::new();
        let (mut survivor, mut peer) = pair(&name.0, api, "client", "graceful", "source");
        peer.send("queue");
        peer.expect("queued");
        peer.send("close");
        peer.expect("closed");
        survivor.send("drain");
        survivor.expect("drained");
        peer.kill();
        survivor.send("after-exit");
        survivor.expect("terminal");
        survivor.finish();

        // Keep both peer processes alive through final-half release and reuse
        // the listener for fresh generations. A detached/uncancelled monitor
        // would hang Drop here, not only at process exit.
        let name = EndpointName::new();
        let (mut survivor, mut peer) =
            pair(&name.0, api, "server", "generations", "generation_source");
        for generation in 0..3 {
            if generation != 0 {
                survivor.expect("connected");
                peer.expect("connected");
            }
            survivor.expect("half-dropped");
            peer.send("send-generation");
            survivor.expect("released");
            peer.expect("released");
            survivor.send("next");
            peer.send("next");
        }
        survivor.finish();
        peer.finish();

        establishment_case(api, "server", "raw-client");
        establishment_case(api, "client", "raw-server");
        establishment_case(api, "client", "raw-server-before-name");
        death_before_accept(api);
    }

    #[cfg(feature = "tokio")]
    {
        let name = EndpointName::new();
        let (mut survivor, mut peer) = pair(&name.0, "tokio", "client", "cancel-recv", "source");
        survivor.expect("cancelled");
        peer.kill();
        survivor.send("resume");
        survivor.expect("terminal");
        survivor.finish();
    }
}

fn establishment_case(api: &str, survivor_role: &str, raw_action: &str) {
    let name = EndpointName::new();
    let (mut survivor, mut peer) = if survivor_role == "server" {
        let survivor = Actor::spawn(&name.0, "server", api, "establish");
        survivor.expect("listening");
        let peer = Actor::spawn(&name.0, "client", "raw", raw_action);
        (survivor, peer)
    } else {
        let peer = Actor::spawn(&name.0, "server", "raw", raw_action);
        peer.expect("listening");
        let survivor = Actor::spawn(&name.0, "client", api, "establish");
        (survivor, peer)
    };
    peer.expect("stalled");
    survivor.remains_pending(); // Neither accept nor connect may expose a connection.
    peer.kill();
    survivor.expect("terminal");
    survivor.finish();
}

fn death_before_accept(api: &str) {
    let name = EndpointName::new();
    let mut survivor = Actor::spawn(&name.0, "server", api, "establish-before-accept");
    survivor.expect("listening");
    let mut peer = Actor::spawn(&name.0, "client", "raw", "raw-client-before-accept");
    peer.expect("stalled");
    peer.kill(); // Reaped before the library obtains the peer PID/native handle.
    survivor.send("accept");
    survivor.expect("terminal");
    survivor.finish();
}

fn event(value: &str) {
    println!("PEER_EVENT {value}");
    std::io::stdout().flush().unwrap();
}

fn command() -> String {
    let mut value = String::new();
    assert_ne!(
        std::io::stdin().read_line(&mut value).unwrap(),
        0,
        "controller disappeared"
    );
    value.trim().to_owned()
}

fn expect_terminal<T>(result: shmem_ipc::Result<T>, graceful: bool) {
    match result {
        Err(Error::ChannelClosed) if graceful => {}
        Err(Error::PeerDisconnected) if !graceful => {}
        Err(err) => panic!("unexpected terminal error: {err:?}"),
        Ok(_) => panic!("operation succeeded after termination"),
    }
}

#[test]
fn peer_child() {
    let Ok(name) = std::env::var("SHMEM_PEER_TEST_NAME") else {
        return;
    };
    let role = std::env::var("SHMEM_PEER_TEST_ROLE").unwrap();
    let api = std::env::var("SHMEM_PEER_TEST_API").unwrap();
    let action = std::env::var("SHMEM_PEER_TEST_ACTION").unwrap();
    match api.as_str() {
        "sync" => sync_child(&name, &role, &action),
        "raw" => raw_child(&name, &action),
        #[cfg(feature = "tokio")]
        "tokio" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async_child(&name, &role, &action)),
        _ => panic!("unknown child API"),
    }
}

fn sync_child(name: &str, role: &str, action: &str) {
    let mut listener = (role == "server").then(|| {
        let mut listener = ShmemListener::bind(name, config()).unwrap();
        if action == "establish-before-accept" {
            assert!(matches!(
                listener.accept_timeout(Duration::ZERO),
                Err(Error::TimedOut)
            ));
        }
        event("listening");
        listener
    });
    if action == "establish-before-accept" {
        assert_eq!(command(), "accept");
    }
    let generations = if action == "generations" || action == "generation_source" {
        3
    } else {
        1
    };
    for generation in 0u32..generations {
        let result = match listener.as_mut() {
            Some(listener) => listener.accept(),
            None => shmem_ipc::connect(name, config()),
        };
        if action == "establish" || action == "establish-before-accept" {
            expect_terminal(result, false);
            event("terminal");
            return;
        }
        let mut conn = result.unwrap();
        event("connected");
        match action {
            "source" => sync_source(conn),
            "recv" | "send" => {
                if action == "send" {
                    conn.send(&FULL_RING).unwrap();
                }
                event("waiting");
                if action == "send" {
                    expect_terminal(conn.send(b"blocked"), false);
                } else {
                    expect_terminal(conn.recv(), false);
                }
                expect_terminal(conn.recv(), false);
                expect_terminal(conn.send(b"after-exit"), false);
                event("terminal");
            }
            "split" => {
                let (tx, mut rx) = conn.split();
                drop(tx);
                event("waiting");
                expect_terminal(rx.recv(), false);
                expect_terminal(rx.recv(), false);
                event("terminal");
            }
            "split-send" => {
                let (mut tx, rx) = conn.split();
                drop(rx);
                tx.send(&FULL_RING).unwrap();
                event("waiting");
                expect_terminal(tx.send(b"blocked"), false);
                expect_terminal(tx.send(b"after-exit"), false);
                event("terminal");
            }
            "drain" | "graceful" => {
                assert_eq!(command(), "drain");
                let graceful = action == "graceful";
                latch_sync_terminal(&mut conn, graceful);
                for expected in QUEUED {
                    assert_eq!(conn.recv().unwrap(), expected);
                }
                assert!(conn.try_recv().unwrap().is_none());
                expect_terminal(conn.recv(), graceful);
                event("drained");
                if graceful {
                    assert_eq!(command(), "after-exit");
                    expect_terminal(conn.send(b"after-exit"), true);
                    expect_terminal(conn.recv(), true);
                    event("terminal");
                }
            }
            "generations" => {
                let (tx, mut rx) = conn.split();
                drop(tx);
                event("half-dropped");
                assert_eq!(rx.recv().unwrap(), generation.to_le_bytes());
                drop(rx);
                event("released");
                assert_eq!(command(), "next");
            }
            "generation_source" => {
                assert_eq!(command(), "send-generation");
                conn.send(&generation.to_le_bytes()).unwrap();
                expect_terminal(conn.recv(), true);
                drop(conn);
                event("released");
                assert_eq!(command(), "next");
            }
            _ => panic!("unknown child action"),
        }
    }
}

fn sync_source(conn: ShmemConnection) {
    let mut conn = Some(conn);
    loop {
        match command().as_str() {
            "queue" => {
                for message in QUEUED {
                    conn.as_mut().unwrap().send(message).unwrap();
                }
                event("queued");
            }
            "close" => {
                drop(conn.take());
                event("closed");
            }
            "crash" => std::process::exit(0),
            _ => panic!("unknown source command"),
        }
    }
}

fn latch_sync_terminal(conn: &mut ShmemConnection, graceful: bool) {
    match conn.send(&FULL_RING) {
        Ok(()) => expect_terminal(conn.send(b"observe-exit"), graceful),
        result => expect_terminal(result, graceful),
    }
}

#[cfg(feature = "tokio")]
async fn pending<F: Future>(future: std::pin::Pin<&mut F>) {
    let mut future = future;
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "operation must be parked"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

#[cfg(feature = "tokio")]
async fn async_child(name: &str, role: &str, action: &str) {
    use shmem_ipc::tokio as ipc;
    let mut listener = if role == "server" {
        let mut listener = ipc::ShmemListener::bind(name, config()).await.unwrap();
        if action == "establish-before-accept" {
            assert!(matches!(
                listener.accept_timeout(Duration::ZERO).await,
                Err(Error::TimedOut)
            ));
        }
        event("listening");
        Some(listener)
    } else {
        None
    };
    if action == "establish-before-accept" {
        assert_eq!(command(), "accept");
    }
    let generations = if action == "generations" || action == "generation_source" {
        3
    } else {
        1
    };
    for generation in 0u32..generations {
        let result = match listener.as_mut() {
            Some(listener) => listener.accept().await,
            None => ipc::connect(name, config()).await,
        };
        if action == "establish" || action == "establish-before-accept" {
            expect_terminal(result, false);
            event("terminal");
            return;
        }
        let mut conn = result.unwrap();
        event("connected");
        match action {
            "source" => {
                let mut conn = Some(conn);
                loop {
                    match command().as_str() {
                        "queue" => {
                            for message in QUEUED {
                                conn.as_mut().unwrap().send(message).await.unwrap();
                            }
                            event("queued");
                        }
                        "close" => {
                            drop(conn.take());
                            event("closed");
                        }
                        "crash" => std::process::exit(0),
                        _ => panic!("unknown source command"),
                    }
                }
            }
            "recv" | "send" | "cancel-recv" => {
                if action == "send" {
                    conn.send(&FULL_RING).await.unwrap();
                    let send = conn.send(b"blocked");
                    tokio::pin!(send);
                    pending(send.as_mut()).await;
                    event("waiting");
                    expect_terminal(send.await, false);
                } else {
                    if action == "cancel-recv" {
                        {
                            let recv = conn.recv();
                            tokio::pin!(recv);
                            pending(recv.as_mut()).await;
                        }
                        event("cancelled");
                        assert_eq!(command(), "resume");
                    } else {
                        let recv = conn.recv();
                        tokio::pin!(recv);
                        pending(recv.as_mut()).await;
                        event("waiting");
                        expect_terminal(recv.await, false);
                    }
                }
                expect_terminal(conn.recv().await, false);
                expect_terminal(conn.send(b"after-exit").await, false);
                event("terminal");
            }
            "split-send" => {
                let (mut tx, rx) = conn.split();
                drop(rx);
                tx.send(&FULL_RING).await.unwrap();
                {
                    let send = tx.send(b"blocked");
                    tokio::pin!(send);
                    pending(send.as_mut()).await;
                    event("waiting");
                    expect_terminal(send.await, false);
                }
                expect_terminal(tx.send(b"after-exit").await, false);
                event("terminal");
            }
            "split" => {
                let (tx, mut rx) = conn.split();
                drop(tx);
                {
                    let recv = rx.recv();
                    tokio::pin!(recv);
                    pending(recv.as_mut()).await;
                    event("waiting");
                    expect_terminal(recv.await, false);
                }
                expect_terminal(rx.recv().await, false);
                event("terminal");
            }
            "drain" | "graceful" => {
                assert_eq!(command(), "drain");
                let graceful = action == "graceful";
                match conn.send(&FULL_RING).await {
                    Ok(()) => expect_terminal(conn.send(b"observe-exit").await, graceful),
                    result => expect_terminal(result, graceful),
                }
                let mut batch = Vec::new();
                assert_eq!(conn.recv_many(&mut batch, 16).await.unwrap(), 3);
                assert_eq!(
                    batch,
                    QUEUED
                        .iter()
                        .map(|message| message.to_vec())
                        .collect::<Vec<_>>()
                );
                assert_eq!(conn.drain_ready(&mut batch, 16).unwrap(), 0);
                assert!(conn.try_recv().unwrap().is_none());
                expect_terminal(conn.recv_many(&mut batch, 16).await, graceful);
                assert_eq!(batch.len(), 3);
                event("drained");
                if graceful {
                    assert_eq!(command(), "after-exit");
                    expect_terminal(conn.send(b"after-exit").await, true);
                    expect_terminal(conn.recv().await, true);
                    event("terminal");
                }
            }
            "generations" => {
                let (tx, mut rx) = conn.split();
                drop(tx);
                event("half-dropped");
                assert_eq!(rx.recv().await.unwrap(), generation.to_le_bytes());
                {
                    let recv = rx.recv();
                    tokio::pin!(recv);
                    pending(recv.as_mut()).await;
                }
                drop(rx); // Cancels/joins the bridge and process monitor while peer lives.
                event("released");
                assert_eq!(command(), "next");
            }
            "generation_source" => {
                assert_eq!(command(), "send-generation");
                conn.send(&generation.to_le_bytes()).await.unwrap();
                expect_terminal(conn.recv().await, true);
                drop(conn);
                event("released");
                assert_eq!(command(), "next");
            }
            _ => panic!("unknown child action"),
        }
    }
}

// Raw bootstrap fixtures intentionally stop before the ordered acknowledgement.
// They do not implement a second transport: all ring operations and all tested
// survivor operations use the public library API.
fn raw_child(name: &str, action: &str) {
    use std::io::Read;
    if action == "raw-client-before-accept" {
        let _stream = raw::connect(name);
        event("stalled");
        let _ = command();
        panic!("raw peer must be terminated externally");
    }
    if action == "raw-client" {
        let mut stream = raw::connect(name);
        let mut len = [0; 2];
        stream.read_exact(&mut len).unwrap();
        let mut bytes = vec![0; u16::from_le_bytes(len) as usize];
        stream.read_exact(&mut bytes).unwrap();
        let conn_name = String::from_utf8(bytes).unwrap();
        let _channel = Channel::open_with_config(&conn_name, config()).unwrap();
        event("stalled");
        let _ = command(); // Controller force-kills us, without publishing Closed.
    } else {
        let listener = raw::bind(name);
        event("listening");
        let mut stream = raw::accept(listener);
        if action != "raw-server-before-name" {
            let conn_name = format!("{name}.raw");
            let _channel = Channel::create_with_config(&conn_name, config()).unwrap();
            stream
                .write_all(&(conn_name.len() as u16).to_le_bytes())
                .unwrap();
            stream.write_all(conn_name.as_bytes()).unwrap();
            stream.flush().unwrap();
            let mut ack = [0];
            stream.read_exact(&mut ack).unwrap();
            assert_eq!(ack, [1]);
            event("stalled");
            let _ = command();
        } else {
            event("stalled");
            let _ = command();
        }
    }
    panic!("raw peer must be terminated externally");
}

#[cfg(unix)]
mod raw {
    use std::os::unix::net::{UnixListener, UnixStream};
    pub fn bind(name: &str) -> UnixListener {
        UnixListener::bind(format!("/tmp/shmem_ipc_{name}.sock")).unwrap()
    }
    pub fn accept(listener: UnixListener) -> UnixStream {
        listener.accept().unwrap().0
    }
    pub fn connect(name: &str) -> UnixStream {
        UnixStream::connect(format!("/tmp/shmem_ipc_{name}.sock")).unwrap()
    }
}

#[cfg(windows)]
mod raw {
    use std::fs::File;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, OPEN_EXISTING, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    fn path(name: &str) -> Vec<u16> {
        format!(r"\\.\pipe\shmem_ipc_{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect()
    }
    pub fn bind(name: &str) -> File {
        let path = path(name);
        let handle = unsafe {
            CreateNamedPipeW(
                path.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(
            handle,
            INVALID_HANDLE_VALUE,
            "{}",
            std::io::Error::last_os_error()
        );
        unsafe { File::from_raw_handle(handle as _) }
    }
    pub fn accept(pipe: File) -> File {
        let ok = unsafe { ConnectNamedPipe(pipe.as_raw_handle() as _, std::ptr::null_mut()) };
        if ok == 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(ERROR_PIPE_CONNECTED as i32)
            );
        }
        pipe
    }
    pub fn connect(name: &str) -> File {
        let path = path(name);
        let deadline = Instant::now() + super::DEADLINE;
        loop {
            let handle = unsafe {
                CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return unsafe { File::from_raw_handle(handle as _) };
            }
            assert!(
                Instant::now() < deadline,
                "{}",
                std::io::Error::last_os_error()
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
