use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, ChannelState, Error, SpinThenWait};

/// 基本的な send/recv
#[test]
fn test_basic_send_recv() {
    let name = "test_basic";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, b"hello");
        client.send(b"world").unwrap();
    });

    server.send(b"hello").unwrap();
    let reply = server.recv().unwrap();
    assert_eq!(reply, b"world");

    handle.join().unwrap();
}

/// 複数メッセージの送受信
#[test]
fn test_multiple_messages() {
    let name = "test_multi";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        for i in 0u32..100 {
            let msg = client.recv().unwrap();
            let expected = i.to_le_bytes();
            assert_eq!(msg, expected);
        }
        client.send(b"done").unwrap();
    });

    for i in 0u32..100 {
        server.send(&i.to_le_bytes()).unwrap();
    }
    let reply = server.recv().unwrap();
    assert_eq!(reply, b"done");

    handle.join().unwrap();
}

/// 大きなメッセージ
#[test]
fn test_large_message() {
    let name = "test_large";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let data = vec![0xABu8; 1024 * 1024]; // 1MB
    let data_clone = data.clone();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, data_clone);
    });

    server.send(&data).unwrap();
    handle.join().unwrap();
}

/// recv_timeout のタイムアウト
#[test]
fn test_recv_timeout() {
    let name = "test_timeout";
    let _ = Channel::cleanup(name);

    let _server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let result = client.recv_timeout(Duration::from_millis(50));
        assert!(matches!(result, Err(Error::TimedOut)));
    });

    // server は何も送らない → client がタイムアウト
    // handle.join() が client スレッドの完了を待つので sleep は不要
    handle.join().unwrap();
}

/// try_recv のノンブロッキング動作
#[test]
fn test_try_recv() {
    let name = "test_try_recv";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();

        // まだ何も来ていない
        let result = client.try_recv().unwrap();
        assert!(result.is_none());
        ready_tx.send(()).unwrap();

        // データが届くまでポーリング (タイミング依存の sleep を排除)
        let deadline = Instant::now() + Duration::from_secs(2);
        let msg = loop {
            if let Some(msg) = client.try_recv().unwrap() {
                break msg;
            }
            assert!(
                Instant::now() < deadline,
                "try_recv timed out waiting for message"
            );
            thread::yield_now();
        };
        assert_eq!(msg, b"ping");
    });

    ready_rx.recv().unwrap();
    server.send(b"ping").unwrap();
    handle.join().unwrap();
}

/// Drop でチャネルが閉じられる
#[test]
fn test_drop_closes_channel() {
    let name = "test_drop";
    let _ = Channel::cleanup(name);

    let server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        // server が drop された後に recv → ChannelClosed
        let result = client.recv_timeout(Duration::from_secs(2));
        assert!(
            matches!(result, Err(Error::ChannelClosed)),
            "expected ChannelClosed, got: {:?}",
            result
        );
    });

    // sleep ではなく state が Connected になるのを待つ (client が open を完了した証拠)
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if server.state() == ChannelState::Connected {
            break;
        }
        assert!(Instant::now() < deadline, "client did not connect in time");
        thread::yield_now();
    }
    drop(server);

    handle.join().unwrap();
}

#[test]
fn test_close_wakes_peer_promptly() {
    let name = "test_close_wakes_peer";
    let _ = Channel::cleanup(name);

    let config = ChannelConfig {
        wait_strategy: SpinThenWait { spin_count: 0 },
        ..Default::default()
    };
    let server = Channel::create_with_config(name, config.clone()).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config).unwrap();
        let start = Instant::now();
        let result = client.recv_timeout(Duration::from_secs(5));
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(Error::ChannelClosed)),
            "expected ChannelClosed, got: {:?}",
            result
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "close wake took too long: {elapsed:.2?}"
        );
    });

    let deadline = Instant::now() + Duration::from_secs(2);
    while server.state() != ChannelState::Connected {
        assert!(Instant::now() < deadline, "client did not connect in time");
        thread::yield_now();
    }

    drop(server);
    handle.join().unwrap();
}

// =========================================================================
// 回帰テスト: Codex レビュー指摘事項
// =========================================================================

/// #2: close 後の send が ChannelClosed を返すこと (post-close write の防止)
#[test]
fn test_send_after_peer_close() {
    let name = "test_send_after_close";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let client = Channel::open(name).unwrap();
        // client を即 drop → state = Closed
        drop(client);
    });

    handle.join().unwrap();

    // peer が閉じた後の send は ChannelClosed を返すべき
    let result = server.send(b"should fail");
    assert!(
        matches!(result, Err(Error::ChannelClosed)),
        "expected ChannelClosed, got: {:?}",
        result
    );
}

/// #3: recv_into でバッファが小さい場合に MessageTooLarge エラー (サイレント切り詰め防止)
#[test]
fn test_recv_into_too_small_buffer() {
    let name = "test_recv_into_small";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();

        // 100 bytes のメッセージを受信しようとするが、バッファは 10 bytes
        let mut small_buf = [0u8; 10];
        let result = client.recv_into(&mut small_buf);
        assert!(
            matches!(result, Err(Error::MessageTooLarge { size: 100, max: 10 })),
            "expected MessageTooLarge, got: {:?}",
            result
        );

        // メッセージはリングから消費済みなので、次のメッセージは受信できる
        let msg = client.recv().unwrap();
        assert_eq!(msg, b"second");
    });

    server.send(&[0xABu8; 100]).unwrap();
    server.send(b"second").unwrap();
    handle.join().unwrap();
}

/// #4: drain セマンティクス — peer close 後もバッファ済みデータが受信できること
#[test]
fn test_drain_after_close() {
    let name = "test_drain";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        client.send(b"msg1").unwrap();
        client.send(b"msg2").unwrap();
        client.send(b"msg3").unwrap();
        // client を drop → state = Closed
        drop(client);
    });

    handle.join().unwrap();

    // peer は閉じたが、バッファ済みの3メッセージは全て受信できるべき
    assert_eq!(server.recv().unwrap(), b"msg1");
    assert_eq!(server.recv().unwrap(), b"msg2");
    assert_eq!(server.recv().unwrap(), b"msg3");

    // バッファが空になったら ChannelClosed
    let result = server.recv_timeout(Duration::from_millis(100));
    assert!(
        matches!(result, Err(Error::ChannelClosed)),
        "expected ChannelClosed after drain, got: {:?}",
        result
    );
}

/// 同名 channel を世代を跨いで再作成しても stale wait object に接続しないことを確認する。
#[test]
fn test_reconnect_same_name_across_generations() {
    let name = "test_reconnect_same_name";
    let _ = Channel::cleanup(name);

    for generation in 0..3u32 {
        let mut server = Channel::create(name).unwrap();
        let request = format!("gen{generation}-req").into_bytes();
        let reply = format!("gen{generation}-ack").into_bytes();
        let expected_request = request.clone();
        let expected_reply = reply.clone();

        let handle = thread::spawn(move || {
            let mut client = Channel::open(name).unwrap();
            let msg = client.recv().unwrap();
            assert_eq!(msg, expected_request);
            client.send(&expected_reply).unwrap();
        });

        server.send(&request).unwrap();
        assert_eq!(server.recv().unwrap(), reply);
        handle.join().unwrap();
    }
}

/// PingPong パフォーマンステスト (回帰テスト用)
#[test]
fn test_ping_pong_performance() {
    let name = "test_perf";
    let _ = Channel::cleanup(name);

    let count = 10_000;
    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let mut buf = vec![0u8; 64];
        for _ in 0..count {
            let n = client.recv_into(&mut buf).unwrap();
            client.send(&buf[..n]).unwrap();
        }
    });

    let msg = vec![0xABu8; 64];
    let start = Instant::now();
    for _ in 0..count {
        server.send(&msg).unwrap();
        let reply = server.recv().unwrap();
        assert_eq!(reply, msg, "echo data mismatch");
    }
    let elapsed = start.elapsed();
    let throughput = (64 * count * 2) as f64 / (1024.0 * 1024.0) / elapsed.as_secs_f64();

    println!(
        "PingPong {count} x 64B: {:.2?} ({:.1} MB/s)",
        elapsed, throughput
    );

    handle.join().unwrap();
}
