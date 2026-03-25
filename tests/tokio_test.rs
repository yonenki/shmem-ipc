#![cfg(feature = "tokio")]

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use shmem_ipc::tokio as ipc_tokio;
use shmem_ipc::{Channel, ChannelConfig, Error, ShmemListener};

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[tokio::test]
async fn tokio_channel_basic_send_recv() {
    let _guard = test_guard();
    let name = "tokio_channel_basic";
    let _ = Channel::cleanup(name);

    let mut server = ipc_tokio::Channel::create(name).await.unwrap();

    let handle = std::thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, b"hello");
        client.send(b"world").unwrap();
    });

    server.send(b"hello").await.unwrap();
    let reply = server.recv().await.unwrap();
    assert_eq!(reply, b"world");

    handle.join().unwrap();
}

#[tokio::test]
async fn tokio_connection_split_send_recv() {
    let _guard = test_guard();
    let name = "tokio_split";
    ShmemListener::cleanup(name);

    let mut listener = ipc_tokio::ShmemListener::bind(name, ChannelConfig::default())
        .await
        .unwrap();

    let client = tokio::spawn(async move {
        let conn = tokio::time::timeout(
            Duration::from_secs(2),
            ipc_tokio::connect(name, ChannelConfig::default()),
        )
        .await
        .expect("client connect timed out")
        .unwrap();
        let (mut tx, mut rx) = conn.split();
        tokio::time::timeout(Duration::from_secs(2), tx.send(b"ping"))
            .await
            .expect("client send timed out")
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("client recv timed out")
            .unwrap();
        assert_eq!(reply, b"pong");
    });

    let conn = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("server accept timed out")
        .unwrap();
    let (mut tx, mut rx) = conn.split();
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("server recv timed out")
        .unwrap();
    assert_eq!(msg, b"ping");
    tokio::time::timeout(Duration::from_secs(2), tx.send(b"pong"))
        .await
        .expect("server send timed out")
        .unwrap();

    client.await.unwrap();
}

#[tokio::test]
async fn tokio_recv_cancel_does_not_advance_cursor() {
    let _guard = test_guard();
    let name = "tokio_recv_cancel";
    let _ = Channel::cleanup(name);

    let mut server = ipc_tokio::Channel::create(name).await.unwrap();

    let handle = std::thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        client.send(b"payload").unwrap();
    });

    {
        let recv_future = server.recv();
        tokio::pin!(recv_future);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            result = &mut recv_future => panic!("recv completed unexpectedly: {result:?}"),
        }
    }

    let msg = server.recv().await.unwrap();
    assert_eq!(msg, b"payload");

    handle.join().unwrap();
}

#[tokio::test]
async fn tokio_send_cancel_does_not_publish() {
    let _guard = test_guard();
    let name = "tokio_send_cancel";
    let _ = Channel::cleanup(name);

    let config = ChannelConfig {
        ring_size: 256,
        ..ChannelConfig::default()
    };
    let mut server = ipc_tokio::Channel::create_with_config(name, config.clone())
        .await
        .unwrap();

    let first = vec![0xAB; 180];
    let second = vec![0xCD; 180];
    let expected_first = first.clone();
    let (start_tx, start_rx) = std::sync::mpsc::channel();

    let handle = std::thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config).unwrap();
        start_rx.recv().unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, expected_first);

        let result = client.recv_timeout(Duration::from_millis(100));
        assert!(
            matches!(result, Err(Error::TimedOut)),
            "expected timeout after cancelled send, got {result:?}"
        );

        client.send(b"ack").unwrap();
    });

    server.send(&first).await.unwrap();

    {
        let send_future = server.send(&second);
        tokio::pin!(send_future);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            result = &mut send_future => panic!("send completed unexpectedly: {result:?}"),
        }
    }

    start_tx.send(()).unwrap();
    let ack = server.recv().await.unwrap();
    assert_eq!(ack, b"ack");

    handle.join().unwrap();
}

#[tokio::test]
async fn tokio_listener_accept_timeout_then_connect() {
    let _guard = test_guard();
    let name = "tokio_listener_timeout";
    ShmemListener::cleanup(name);

    let mut listener = ipc_tokio::ShmemListener::bind(name, ChannelConfig::default())
        .await
        .unwrap();

    let timeout = listener.accept_timeout(Duration::from_millis(50)).await;
    assert!(matches!(timeout, Err(Error::TimedOut)));

    let client = tokio::spawn(async move {
        let mut conn = tokio::time::timeout(
            Duration::from_secs(2),
            ipc_tokio::connect(name, ChannelConfig::default()),
        )
        .await
        .expect("client2 connect timed out")
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), conn.send(b"hello"))
            .await
            .expect("client2 send timed out")
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(2), conn.recv())
            .await
            .expect("client2 recv timed out")
            .unwrap();
        assert_eq!(reply, b"world");
    });

    let mut conn = tokio::time::timeout(Duration::from_secs(2), listener.accept())
        .await
        .expect("server2 accept timed out")
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), conn.recv())
        .await
        .expect("server2 recv timed out")
        .unwrap();
    assert_eq!(msg, b"hello");
    tokio::time::timeout(Duration::from_secs(2), conn.send(b"world"))
        .await
        .expect("server2 send timed out")
        .unwrap();

    client.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tokio_channel_ping_pong_stress() {
    let _guard = test_guard();
    let name = "tokio_ping_pong_stress";
    let _ = Channel::cleanup(name);

    let mut server = ipc_tokio::Channel::create(name).await.unwrap();
    let rounds = 10_000usize;

    let client = tokio::spawn(async move {
        let mut client = ipc_tokio::Channel::open(name).await.unwrap();
        let mut buf = vec![0u8; 64];
        for round in 0..rounds {
            let n = tokio::time::timeout(Duration::from_secs(2), client.recv_into(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("client recv timed out at round {round}"))
                .unwrap_or_else(|err| panic!("client recv failed at round {round}: {err:?}"));
            tokio::time::timeout(Duration::from_secs(2), client.send(&buf[..n]))
                .await
                .unwrap_or_else(|_| panic!("client send timed out at round {round}"))
                .unwrap_or_else(|err| panic!("client send failed at round {round}: {err:?}"));
        }
    });

    let payload = [0xABu8; 64];
    let mut recv_buf = [0u8; 64];
    for round in 0..rounds {
        tokio::time::timeout(Duration::from_secs(2), server.send(&payload))
            .await
            .unwrap_or_else(|_| panic!("server send timed out at round {round}"))
            .unwrap_or_else(|err| panic!("server send failed at round {round}: {err:?}"));
        let n = tokio::time::timeout(Duration::from_secs(2), server.recv_into(&mut recv_buf))
            .await
            .unwrap_or_else(|_| panic!("server recv timed out at round {round}"))
            .unwrap_or_else(|err| panic!("server recv failed at round {round}: {err:?}"));
        assert_eq!(&recv_buf[..n], &payload);
    }

    client.await.unwrap();
}

#[tokio::test]
async fn tokio_recv_many_drains_available_messages() {
    let _guard = test_guard();
    let name = "tokio_recv_many";
    let _ = Channel::cleanup(name);

    let mut server = ipc_tokio::Channel::create(name).await.unwrap();

    let handle = std::thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        client.send(b"one").unwrap();
        client.send(b"two").unwrap();
        client.send(b"three").unwrap();
    });

    let mut batch = Vec::new();
    let count = server.recv_many(&mut batch, 8).await.unwrap();
    assert_eq!(count, 3);
    assert_eq!(
        batch,
        vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
    );

    let drained = server.drain_ready(&mut batch, 8).unwrap();
    assert_eq!(drained, 0);

    handle.join().unwrap();
}
