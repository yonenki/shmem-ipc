//! shmem-ipc ストレステスト
//!
//! 実装固有のエッジケースと一般的な IPC のエッジケースを網羅的にテスト。
//! 全テスト通過 = 実装の堅牢性が確認できる。
//!
//! Usage:
//!   cargo run --release --example stress

use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, Error, SpinThenWait};

fn config(ring_size: usize, spin_count: u32) -> ChannelConfig {
    ChannelConfig {
        ring_size,
        wait_strategy: SpinThenWait { spin_count },
        ..Default::default()
    }
}

// =========================================================================
// 1. Ring wrap-around boundary
// =========================================================================
fn test_wrap_around() {
    println!("[1] Ring wrap-around boundary...");
    let name = "stress_wrap";
    let _ = Channel::cleanup(name);

    // 4096 byte リングで wrap を頻発させる
    let ring_size = 4096;
    let mut server = Channel::create_with_config(name, config(ring_size, 512)).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config(ring_size, 512)).unwrap();

        for _round in 0..10_000u32 {
            let msg = client.recv().unwrap();
            // エコー (データ検証は server 側で行う)
            client.send(&msg).unwrap();
        }
    });

    // さまざまなサイズで wrap 境界を突く
    let sizes = [1, 7, 8, 15, 32, 64, 100, 200, 500, 1000, 2000, 3000];
    for round in 0..10_000u32 {
        let payload_size = sizes[round as usize % sizes.len()];
        let mut payload = vec![0u8; payload_size];
        // 4byte 以上なら round 番号を先頭に埋め込む
        if payload_size >= 4 {
            payload[..4].copy_from_slice(&round.to_le_bytes());
        }
        // 残りを deterministic パターンで埋める
        let start = 4.min(payload_size);
        for (i, b) in payload[start..].iter_mut().enumerate() {
            *b = ((round as usize + i) & 0xFF) as u8;
        }
        server.send(&payload).unwrap();
        let reply = server.recv().unwrap();
        assert_eq!(reply, payload, "wrap-around echo mismatch at round {round}");
    }

    handle.join().unwrap();
    println!("  PASS (10,000 rounds, varying sizes)");
}

// =========================================================================
// 2. Near-full ring + backpressure
// =========================================================================
fn test_backpressure() {
    println!("[2] Near-full ring + backpressure...");
    let name = "stress_bp";
    let _ = Channel::cleanup(name);

    let ring_size = 65536;
    // spin_count=0 で即座に futex に落とす → parked フラグが必ず使われる
    let mut server = Channel::create_with_config(name, config(ring_size, 0)).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config(ring_size, 0)).unwrap();
        // 遅い consumer: 20ms ごとに 4 メッセージ drain
        let mut received = 0;
        while received < 64 {
            thread::sleep(Duration::from_millis(20));
            for _ in 0..4 {
                let msg = client.recv_timeout(Duration::from_secs(2)).unwrap();
                let round = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
                assert_eq!(round, received as u32);
                received += 1;
                if received >= 64 {
                    break;
                }
            }
        }
    });

    let mut blocked_count = 0;
    for i in 0u32..64 {
        let mut payload = vec![0u8; 4096];
        payload[..4].copy_from_slice(&i.to_le_bytes());

        let start = Instant::now();
        server
            .send_timeout(&payload, Duration::from_secs(5))
            .unwrap();
        if start.elapsed() > Duration::from_millis(5) {
            blocked_count += 1;
        }
    }

    handle.join().unwrap();
    assert!(
        blocked_count > 0,
        "expected at least one blocked send, got {blocked_count}"
    );
    println!("  PASS ({blocked_count} sends blocked by backpressure)");
}

// =========================================================================
// 3. Zero-length messages
// =========================================================================
fn test_zero_length() {
    println!("[3] Zero-length messages...");
    let name = "stress_zero";
    let _ = Channel::cleanup(name);

    let ring_size = 4096; // 小リングで header-only フレームの wrap を強制
    let count = 50_000;
    let mut server = Channel::create_with_config(name, config(ring_size, 512)).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config(ring_size, 512)).unwrap();
        for _ in 0..count {
            let msg = client.recv().unwrap();
            assert_eq!(msg.len(), 0, "zero-length message has non-zero length");
        }
    });

    for _ in 0..count {
        server.send(b"").unwrap();
    }

    handle.join().unwrap();
    println!("  PASS ({count} zero-length messages)");
}

// =========================================================================
// 4. Maximum message size
// =========================================================================
fn test_max_message() {
    println!("[4] Maximum message size...");
    let name = "stress_maxmsg";
    let _ = Channel::cleanup(name);

    let ring_size = 65536;
    let max_payload = ring_size - 8; // MSG_HEADER_SIZE = 8
    let mut server = Channel::create_with_config(name, config(ring_size, 512)).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config(ring_size, 512)).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg.len(), max_payload);
        // checksum
        for (i, &b) in msg.iter().enumerate() {
            assert_eq!(b, (i & 0xFF) as u8, "max message data corruption at {i}");
        }
    });

    // ちょうど最大
    let mut payload = vec![0u8; max_payload];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    server.send(&payload).unwrap();

    // 1 byte 超過 → エラー
    let result = server.send(&vec![0u8; max_payload + 1]);
    assert!(matches!(result, Err(Error::MessageTooLarge { .. })));

    handle.join().unwrap();
    println!("  PASS (max={max_payload} bytes)");
}

// =========================================================================
// 5. Mixed tiny and huge messages
// =========================================================================
fn test_mixed_sizes() {
    println!("[5] Mixed tiny and huge messages...");
    let name = "stress_mixed";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let sizes = [0, 1, 7, 8, 15, 64, 4096, 65536, 1_048_576, 2, 32, 3, 0];
    let cycles = 50;
    let total = sizes.len() * cycles;

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        for cycle in 0..cycles {
            for (slot, &size) in sizes.iter().enumerate() {
                let msg = client.recv().unwrap();
                assert_eq!(
                    msg.len(),
                    size,
                    "size mismatch at cycle={cycle} slot={slot}"
                );
                if size >= 8 {
                    let got_cycle = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]) as usize;
                    let got_slot = u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]]) as usize;
                    assert_eq!(got_cycle, cycle);
                    assert_eq!(got_slot, slot);
                }
            }
        }
    });

    for cycle in 0..cycles {
        for (slot, &size) in sizes.iter().enumerate() {
            let mut payload = vec![0u8; size];
            if size >= 8 {
                payload[..4].copy_from_slice(&(cycle as u32).to_le_bytes());
                payload[4..8].copy_from_slice(&(slot as u32).to_le_bytes());
            }
            for i in 8..size {
                payload[i] = ((cycle + slot + i) & 0xFF) as u8;
            }
            server.send(&payload).unwrap();
        }
    }

    handle.join().unwrap();
    println!("  PASS ({total} messages, sizes {:?})", sizes);
}

// =========================================================================
// 6. Thundering herd (backlog burst drain)
// =========================================================================
fn test_backlog_burst() {
    println!("[6] Backlog burst drain...");
    let name = "stress_burst";
    let _ = Channel::cleanup(name);

    let count = 100_000;
    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        // 25ms 寝てから一気に drain
        thread::sleep(Duration::from_millis(25));
        let start = Instant::now();
        for i in 0u32..count {
            let msg = client.try_recv();
            match msg {
                Ok(Some(data)) => {
                    let got = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    assert_eq!(got, i, "out of order at {i}");
                }
                Ok(None) => {
                    // まだ来ていない → recv で待つ
                    let data = client.recv().unwrap();
                    let got = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                    assert_eq!(got, i, "out of order at {i}");
                }
                Err(e) => panic!("recv error at {i}: {e}"),
            }
        }
        let elapsed = start.elapsed();
        println!("  drain: {count} msgs in {elapsed:.2?}");
    });

    // 一気に送信 (consumer が寝ている間)
    for i in 0u32..count {
        let mut payload = vec![0u8; 64];
        payload[..4].copy_from_slice(&i.to_le_bytes());
        server.send(&payload).unwrap();
    }

    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
// 7. Close during active recv
// =========================================================================
fn test_close_during_recv() {
    println!("[7] Close during active recv...");
    let name = "stress_close_recv";
    let _ = Channel::cleanup(name);

    let server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let start = Instant::now();
        let result = client.recv_timeout(Duration::from_secs(5));
        let elapsed = start.elapsed();
        assert!(
            matches!(result, Err(Error::ChannelClosed)),
            "expected ChannelClosed, got: {:?}",
            result
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "close detection took too long: {elapsed:.2?}"
        );
    });

    // client が recv_timeout に入るのを待つ
    thread::sleep(Duration::from_millis(50));
    drop(server); // close → futex_wake で即座に ChannelClosed

    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
// 8. Close during active send (ring full)
// =========================================================================
fn test_close_during_send() {
    println!("[8] Close during active send (ring full)...");
    let name = "stress_close_send";
    let _ = Channel::cleanup(name);

    // 小さいリングを最大サイズのメッセージで埋めて、確実にブロックさせる
    let ring_size = 4096;
    let max_payload = ring_size - 8; // MSG_HEADER_SIZE = 8
    let mut server = Channel::create_with_config(name, config(ring_size, 0)).unwrap();

    let handle = thread::spawn(move || {
        let client = Channel::open_with_config(name, config(ring_size, 0)).unwrap();
        // recv しないので ring が詰まる → server の send がブロック
        thread::sleep(Duration::from_millis(100));
        drop(client); // close
    });

    // ring を1メッセージで完全に埋める (max_payload + 8 = ring_size, 空き 0)
    let payload = vec![0u8; max_payload];
    server.send(&payload).unwrap();

    // 2回目は ring が完全に埋まっているので必ずブロック
    let start = Instant::now();
    let result = server.send_timeout(&vec![0u8; 1], Duration::from_secs(5));
    let elapsed = start.elapsed();

    assert!(
        matches!(result, Err(Error::ChannelClosed)),
        "expected ChannelClosed, got: {:?}",
        result
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "close detection took too long: {elapsed:.2?}"
    );

    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
// 9. Drain after close (buffered data must be delivered)
// =========================================================================
fn test_drain_after_close() {
    println!("[9] Drain after close...");
    let name = "stress_drain";
    let _ = Channel::cleanup(name);

    let mut server = Channel::create(name).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        for i in 0u32..1000 {
            let mut payload = vec![0u8; 1024];
            payload[..4].copy_from_slice(&i.to_le_bytes());
            client.send(&payload).unwrap();
        }
        // 全部送ったら即 drop
        drop(client);
    });

    handle.join().unwrap();

    // peer は閉じたが、バッファ済み 1000 メッセージは全て受信できるべき
    for i in 0u32..1000 {
        let msg = server.recv().unwrap();
        let got = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
        assert_eq!(got, i, "drain order mismatch at {i}");
    }

    // 空になったら ChannelClosed
    let result = server.recv_timeout(Duration::from_millis(100));
    assert!(matches!(result, Err(Error::ChannelClosed)));

    println!("  PASS (1000 messages drained after close)");
}

// =========================================================================
// 10. Timeout accuracy
// =========================================================================
fn test_timeout_accuracy() {
    println!("[10] Timeout accuracy...");
    let name = "stress_timeout";
    let _ = Channel::cleanup(name);

    let _server = Channel::create_with_config(name, config(16 * 1024 * 1024, 0)).unwrap();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(name, config(16 * 1024 * 1024, 0)).unwrap();

        let mut durations = Vec::new();
        let timeout = Duration::from_millis(5);

        for _ in 0..100 {
            let start = Instant::now();
            let _ = client.recv_timeout(timeout);
            durations.push(start.elapsed());
        }

        durations.sort();
        let p50 = durations[50];
        let p99 = durations[99];
        let min = durations[0];

        println!("  min={min:.2?}, p50={p50:.2?}, p99={p99:.2?} (target=5ms)");
        assert!(
            min >= Duration::from_millis(4),
            "timeout returned too early: {min:.2?}"
        );
        assert!(p50 < Duration::from_millis(15), "p50 too high: {p50:.2?}");
    });

    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
// main
// =========================================================================
// 11. Server drop before Client open
// =========================================================================
fn test_server_drop_before_open() {
    println!("[11] Server drop before Client open...");
    let name = "stress_srv_drop";
    let _ = Channel::cleanup(name);

    // Server を作って即 drop → ファイル削除される
    let _server = Channel::create(name).unwrap();
    drop(_server);

    // Client が open → ファイルがないのでタイムアウトすべき
    let config = ChannelConfig {
        connect_timeout: Duration::from_millis(200),
        ..config(4096, 512)
    };
    let result = Channel::open_with_config(name, config).err();
    assert!(
        matches!(result, Some(Error::TimedOut)),
        "expected TimedOut when server already dropped, got: {:?}",
        result
    );
    println!("  PASS");
}

// =========================================================================
// 12. Dual Client open (SPSC: 2番目は拒否すべき)
// =========================================================================
fn test_dual_client_open() {
    println!("[12] Dual Client open (SPSC rejection)...");
    let name = "stress_dual";
    let _ = Channel::cleanup(name);

    let _server = Channel::create(name).unwrap();

    // 1番目の Client: Connected に遷移
    let _client1 = Channel::open(name).unwrap();

    // 2番目の Client: state が既に Connected なので ServerReady → Connected の CAS が失敗すべき
    let config = ChannelConfig {
        connect_timeout: Duration::from_millis(200),
        ..Default::default()
    };
    let result = Channel::open_with_config(name, config).err();
    assert!(
        matches!(result, Some(Error::ChannelClosed)),
        "expected ChannelClosed for second client, got: {:?}",
        result
    );
    println!("  PASS");
}

// =========================================================================
// 13. Reconnection (同じ名前で再接続)
// =========================================================================
fn test_reconnection() {
    println!("[13] Reconnection (same name)...");
    let name = "stress_reconnect";
    let _ = Channel::cleanup(name);

    // 1回目の接続
    {
        let mut server = Channel::create(name).unwrap();
        let handle = thread::spawn(move || {
            let mut client = Channel::open(name).unwrap();
            let msg = client.recv().unwrap();
            assert_eq!(msg, b"gen1");
            client.send(b"ack1").unwrap();
        });
        server.send(b"gen1").unwrap();
        let reply = server.recv().unwrap();
        assert_eq!(reply, b"ack1");
        handle.join().unwrap();
    }
    // server & client が drop → ファイル削除

    // 2回目の接続 (同じ名前)
    {
        let mut server = Channel::create(name).unwrap();
        let handle = thread::spawn(move || {
            let mut client = Channel::open(name).unwrap();
            let msg = client.recv().unwrap();
            assert_eq!(msg, b"gen2");
            client.send(b"ack2").unwrap();
        });
        server.send(b"gen2").unwrap();
        let reply = server.recv().unwrap();
        assert_eq!(reply, b"ack2");
        handle.join().unwrap();
    }

    println!("  PASS (2 generations)");
}

// =========================================================================
// 14. Client open before Server (timeout)
// =========================================================================
fn test_client_open_before_server() {
    println!("[14] Client open before Server...");
    let name = "stress_early_client";
    let _ = Channel::cleanup(name);

    let handle = thread::spawn(move || {
        // Client が先に open を試みる → Server がいないのでポーリング
        let mut client = Channel::open(name).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, b"late_hello");
    });

    // 200ms 後に Server を作成
    thread::sleep(Duration::from_millis(200));
    let mut server = Channel::create(name).unwrap();
    // Client が接続するのを少し待つ
    thread::sleep(Duration::from_millis(100));
    server.send(b"late_hello").unwrap();

    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
// 15. Server drop during Client open polling
// =========================================================================
fn test_stale_file_rejected() {
    println!("[15] Stale file from previous session rejected...");
    let name = "stress_stale";
    let _ = Channel::cleanup(name);

    // 1回目: Server を作って Client が接続 → 両方 drop → state=Closed
    // ただし Client の drop ではファイルを削除しない (Server のみ削除)
    // ここでは Server が削除するので、2回目に残骸が残る状況を作る
    //
    // 別のアプローチ: Client 側で drop → Server はまだ生きているが close 状態
    {
        let _server = Channel::create(name).unwrap();
        let _client = Channel::open(name).unwrap();
        // client drop → state=Closed, ファイルは残る (client は削除しない)
        drop(_client);
        // server は state=Closed のファイルを持ったまま
        // server drop → ファイル削除
    }

    // Server が新しく create → truncate で上書き → 正常に動くべき
    let mut server = Channel::create(name).unwrap();
    let handle = thread::spawn(move || {
        let mut client = Channel::open(name).unwrap();
        let msg = client.recv().unwrap();
        assert_eq!(msg, b"fresh");
    });
    server.send(b"fresh").unwrap();
    handle.join().unwrap();
    println!("  PASS");
}

// =========================================================================
fn main() {
    println!("shmem-ipc stress test suite");
    println!("===========================\n");

    let start = Instant::now();

    test_wrap_around();
    test_backpressure();
    test_zero_length();
    test_max_message();
    test_mixed_sizes();
    test_backlog_burst();
    test_close_during_recv();
    test_close_during_send();
    test_drain_after_close();
    test_timeout_accuracy();
    test_server_drop_before_open();
    test_dual_client_open();
    test_reconnection();
    test_client_open_before_server();
    test_stale_file_rejected();

    let elapsed = start.elapsed();
    println!("\nAll stress tests passed in {elapsed:.2?}");
}
