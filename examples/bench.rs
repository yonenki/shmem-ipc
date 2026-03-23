//! shmem-ipc ベンチマーク
//!
//! 3つの通信パターンで計測:
//! 1. PingPong   — 1メッセージ送信 → 応答待ち (レイテンシ重視)
//! 2. Burst      — N メッセージを一気に送信 → まとめて受信 (バッファリング特性)
//! 3. Streaming  — 片方が連続送信、もう片方が連続受信 (スループット重視)
//!
//! Usage:
//!   cargo run --release --example bench
//!   cargo run --release --example bench -- pingpong
//!   cargo run --release --example bench -- burst
//!   cargo run --release --example bench -- stream

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, SpinThenWait};

const ROUNDS: usize = 5;
const SPIN_COUNT: u32 = 256;

fn make_config() -> ChannelConfig {
    ChannelConfig {
        wait_strategy: SpinThenWait { spin_count: SPIN_COUNT },
        ..Default::default()
    }
}

// =========================================================================
// PingPong — 送信→応答→送信→応答 (RTT レイテンシ計測)
// =========================================================================

fn bench_pingpong(name: &str, msg_size: usize, count: usize, warmup: usize) {
    let ch_name = format!("bench_pp_{name}");
    let _ = Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config()).unwrap();
    let ch_name_clone = ch_name.clone();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(&ch_name_clone, make_config()).unwrap();
        let mut buf = vec![0u8; msg_size];
        for _ in 0..(warmup + ROUNDS * count) {
            let n = client.recv_into(&mut buf).unwrap();
            client.send(&buf[..n]).unwrap();
        }
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut recv_buf = vec![0u8; msg_size];

    for _ in 0..warmup {
        server.send(&send_buf).unwrap();
        server.recv_into(&mut recv_buf).unwrap();
    }

    let mut results: Vec<(Duration, f64)> = Vec::new();

    for r in 0..ROUNDS {
        let mut latencies = Vec::with_capacity(count);
        let total_start = Instant::now();

        for _ in 0..count {
            let start = Instant::now();
            server.send(&send_buf).unwrap();
            server.recv_into(&mut recv_buf).unwrap();
            latencies.push(start.elapsed());
        }

        let total = total_start.elapsed();
        latencies.sort();
        let p50 = latencies[count / 2];
        let throughput = (msg_size as u64 * count as u64 * 2) as f64
            / (1024.0 * 1024.0)
            / total.as_secs_f64();

        println!("  Round {}/{}: p50={:.2?}, {:.1} MB/s", r + 1, ROUNDS, p50, throughput);
        results.push((p50, throughput));
    }

    handle.join().unwrap();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (p50, tput) = results[ROUNDS / 2];
    println!("=== PingPong {name} ({msg_size}B x {count}) === p50={p50:.2?}, {tput:.1} MB/s\n");
}

// =========================================================================
// Burst — N メッセージ一気送り → まとめて受信 (バッファリング & バックプレッシャー)
// =========================================================================

fn bench_burst(name: &str, msg_size: usize, burst_size: usize, bursts: usize) {
    let ch_name = format!("bench_burst_{name}");
    let _ = Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config()).unwrap();
    let ch_name_clone = ch_name.clone();
    let total_msgs = ROUNDS * bursts * burst_size;

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(&ch_name_clone, make_config()).unwrap();
        let mut buf = vec![0u8; msg_size];
        for _ in 0..total_msgs {
            client.recv_into(&mut buf).unwrap();
        }
        // 完了通知
        client.send(b"done").unwrap();
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut results: Vec<(Duration, f64)> = Vec::new();

    for r in 0..ROUNDS {
        let total_start = Instant::now();

        for _ in 0..bursts {
            // バースト: burst_size 個を一気に送信
            for _ in 0..burst_size {
                server.send(&send_buf).unwrap();
            }
        }

        let total = total_start.elapsed();
        let total_bytes = msg_size as u64 * bursts as u64 * burst_size as u64;
        let throughput = total_bytes as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!("  Round {}/{}: {:.2?}, {:.1} MB/s", r + 1, ROUNDS, total, throughput);
        results.push((total, throughput));
    }

    // client の完了を待つ
    server.recv().unwrap();
    handle.join().unwrap();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (_, tput) = results[ROUNDS / 2];
    println!(
        "=== Burst {name} ({msg_size}B x {burst_size} x {bursts} bursts) === {tput:.1} MB/s\n"
    );
}

// =========================================================================
// Streaming — 一方向の連続送信 (最大スループット計測)
// =========================================================================

fn bench_stream(name: &str, msg_size: usize, count: usize) {
    let ch_name = format!("bench_stream_{name}");
    let _ = Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config()).unwrap();
    let ch_name_clone = ch_name.clone();
    let total_msgs = ROUNDS * count;

    // receiver スレッド: 受信カウントと所要時間を計測
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = done.clone();

    let handle = thread::spawn(move || {
        let mut client = Channel::open_with_config(&ch_name_clone, make_config()).unwrap();
        let mut buf = vec![0u8; msg_size];
        let mut received = 0u64;
        let start = Instant::now();

        while !done_clone.load(Ordering::Relaxed) || received < total_msgs as u64 {
            match client.recv_into(&mut buf) {
                Ok(_) => received += 1,
                Err(_) => break,
            }
            if received >= total_msgs as u64 {
                break;
            }
        }

        let elapsed = start.elapsed();
        (received, elapsed)
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut results: Vec<(Duration, f64)> = Vec::new();

    for r in 0..ROUNDS {
        let total_start = Instant::now();

        for _ in 0..count {
            server.send(&send_buf).unwrap();
        }

        let total = total_start.elapsed();
        let total_bytes = msg_size as u64 * count as u64;
        let throughput = total_bytes as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!("  Round {}/{}: {:.2?}, {:.1} MB/s (send)", r + 1, ROUNDS, total, throughput);
        results.push((total, throughput));
    }

    done.store(true, Ordering::Relaxed);
    let (received, recv_elapsed) = handle.join().unwrap();
    let recv_throughput =
        (msg_size as u64 * received) as f64 / (1024.0 * 1024.0) / recv_elapsed.as_secs_f64();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (_, tput) = results[ROUNDS / 2];
    println!(
        "=== Stream {name} ({msg_size}B x {count}) === send: {tput:.1} MB/s, recv: {recv_throughput:.1} MB/s ({received} msgs)\n"
    );
}

// =========================================================================
// main
// =========================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("all");

    let sizes: &[(& str, usize)] = &[
        ("64B", 64),
        ("256B", 256),
        ("4KB", 4_096),
        ("64KB", 65_536),
        ("256KB", 262_144),
        ("1MB", 1_048_576),
        ("10MB", 10_485_760),
    ];

    if mode == "all" || mode == "pingpong" {
        println!("============ PingPong (RTT latency) ============\n");
        for &(name, size) in sizes {
            let count = match size {
                s if s <= 256 => 100_000,
                s if s <= 4_096 => 50_000,
                s if s <= 65_536 => 10_000,
                s if s <= 262_144 => 1_000,
                s if s <= 1_048_576 => 100,
                _ => 20,
            };
            bench_pingpong(name, size, count, count / 100);
        }
    }

    if mode == "all" || mode == "burst" {
        println!("============ Burst (buffering) ============\n");
        for &(name, size) in sizes {
            let (burst_size, bursts) = match size {
                s if s <= 256 => (1000, 100),
                s if s <= 4_096 => (500, 50),
                s if s <= 65_536 => (100, 20),
                s if s <= 262_144 => (50, 10),
                s if s <= 1_048_576 => (10, 5),
                _ => (5, 3),
            };
            bench_burst(name, size, burst_size, bursts);
        }
    }

    if mode == "all" || mode == "stream" {
        println!("============ Streaming (one-way throughput) ============\n");
        for &(name, size) in sizes {
            let count = match size {
                s if s <= 256 => 500_000,
                s if s <= 4_096 => 100_000,
                s if s <= 65_536 => 20_000,
                s if s <= 262_144 => 5_000,
                s if s <= 1_048_576 => 1_000,
                _ => 200,
            };
            bench_stream(name, size, count);
        }
    }
}
