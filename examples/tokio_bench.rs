use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use shmem_ipc::ChannelConfig;
use shmem_ipc::tokio::Channel;

const ROUNDS: usize = 5;

fn make_config() -> ChannelConfig {
    ChannelConfig::default()
}

fn bench_scale() -> f64 {
    std::env::var("SHMEM_BENCH_SCALE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(1.0)
}

fn scaled_count(base: usize, scale: f64) -> usize {
    ((base as f64 * scale).round() as usize).max(1)
}

fn bench_filter() -> Option<String> {
    std::env::var("SHMEM_BENCH_FILTER")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
}

fn runtime_mode() -> String {
    std::env::var("SHMEM_TOKIO_RUNTIME")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value == "current_thread" || value == "multi_thread")
        .unwrap_or_else(|| "multi_thread".to_string())
}

fn worker_threads() -> usize {
    std::env::var("SHMEM_TOKIO_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(2)
}

async fn bench_pingpong(name: &str, msg_size: usize, count: usize, warmup: usize) {
    let ch_name = format!("tokio_bench_pp_{name}");
    let _ = shmem_ipc::Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config())
        .await
        .unwrap();
    let client_name = ch_name.clone();

    let client = tokio::spawn(async move {
        let mut client = Channel::open_with_config(&client_name, make_config())
            .await
            .unwrap();
        let mut buf = vec![0u8; msg_size];
        for _ in 0..(warmup + ROUNDS * count) {
            let n = client.recv_into(&mut buf).await.unwrap();
            client.send(&buf[..n]).await.unwrap();
        }
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut recv_buf = vec![0u8; msg_size];

    for _ in 0..warmup {
        server.send(&send_buf).await.unwrap();
        server.recv_into(&mut recv_buf).await.unwrap();
    }

    let mut results = Vec::new();
    for round in 0..ROUNDS {
        let mut latencies = Vec::with_capacity(count);
        let total_start = Instant::now();

        for _ in 0..count {
            let start = Instant::now();
            server.send(&send_buf).await.unwrap();
            server.recv_into(&mut recv_buf).await.unwrap();
            latencies.push(start.elapsed());
        }

        let total = total_start.elapsed();
        latencies.sort();
        let p50 = latencies[count / 2];
        let throughput =
            (msg_size as u64 * count as u64 * 2) as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!(
            "  Round {}/{}: p50={:.2?}, {:.1} MB/s",
            round + 1,
            ROUNDS,
            p50,
            throughput
        );
        results.push((p50, throughput));
    }

    client.await.unwrap();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (p50, throughput) = results[ROUNDS / 2];
    println!(
        "=== Tokio PingPong {name} ({msg_size}B x {count}) === p50={p50:.2?}, {throughput:.1} MB/s\n"
    );
}

async fn bench_burst(name: &str, msg_size: usize, burst_size: usize, bursts: usize) {
    let ch_name = format!("tokio_bench_burst_{name}");
    let _ = shmem_ipc::Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config())
        .await
        .unwrap();
    let client_name = ch_name.clone();
    let total_msgs = ROUNDS * bursts * burst_size;

    let client = tokio::spawn(async move {
        let mut client = Channel::open_with_config(&client_name, make_config())
            .await
            .unwrap();
        let mut buf = vec![0u8; msg_size];
        for _ in 0..total_msgs {
            client.recv_into(&mut buf).await.unwrap();
        }
        client.send(b"done").await.unwrap();
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut results = Vec::new();

    for round in 0..ROUNDS {
        let total_start = Instant::now();
        for _ in 0..bursts {
            for _ in 0..burst_size {
                server.send(&send_buf).await.unwrap();
            }
        }
        let total = total_start.elapsed();
        let total_bytes = msg_size as u64 * bursts as u64 * burst_size as u64;
        let throughput = total_bytes as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!(
            "  Round {}/{}: {:.2?}, {:.1} MB/s",
            round + 1,
            ROUNDS,
            total,
            throughput
        );
        results.push((total, throughput));
    }

    server.recv().await.unwrap();
    client.await.unwrap();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (_, throughput) = results[ROUNDS / 2];
    println!(
        "=== Tokio Burst {name} ({msg_size}B x {burst_size} x {bursts} bursts) === {throughput:.1} MB/s\n"
    );
}

async fn bench_stream(name: &str, msg_size: usize, count: usize) {
    let ch_name = format!("tokio_bench_stream_{name}");
    let _ = shmem_ipc::Channel::cleanup(&ch_name);

    let mut server = Channel::create_with_config(&ch_name, make_config())
        .await
        .unwrap();
    let client_name = ch_name.clone();
    let total_msgs = ROUNDS * count;
    let done = Arc::new(AtomicBool::new(false));
    let done_clone = Arc::clone(&done);

    let client = tokio::spawn(async move {
        let mut client = Channel::open_with_config(&client_name, make_config())
            .await
            .unwrap();
        let mut buf = vec![0u8; msg_size];
        let mut received = 0u64;
        let start = Instant::now();

        while !done_clone.load(Ordering::Relaxed) || received < total_msgs as u64 {
            match client.recv_into(&mut buf).await {
                Ok(_) => received += 1,
                Err(_) => break,
            }
            if received >= total_msgs as u64 {
                break;
            }
        }

        (received, start.elapsed())
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut results = Vec::new();

    for round in 0..ROUNDS {
        let total_start = Instant::now();
        for _ in 0..count {
            server.send(&send_buf).await.unwrap();
        }
        let total = total_start.elapsed();
        let total_bytes = msg_size as u64 * count as u64;
        let throughput = total_bytes as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!(
            "  Round {}/{}: {:.2?}, {:.1} MB/s (send)",
            round + 1,
            ROUNDS,
            total,
            throughput
        );
        results.push((total, throughput));
    }

    done.store(true, Ordering::Relaxed);
    let (received, recv_elapsed) = client.await.unwrap();
    let recv_throughput =
        (msg_size as u64 * received) as f64 / (1024.0 * 1024.0) / recv_elapsed.as_secs_f64();

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let (_, throughput) = results[ROUNDS / 2];
    println!(
        "=== Tokio Stream {name} ({msg_size}B x {count}) === send: {throughput:.1} MB/s, recv: {recv_throughput:.1} MB/s ({received} msgs)\n"
    );
}

async fn run() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("all");
    let scale = bench_scale();
    let filter = bench_filter();
    let runtime = runtime_mode();

    let sizes: &[(&str, usize)] = &[
        ("64B", 64),
        ("256B", 256),
        ("4KB", 4_096),
        ("64KB", 65_536),
        ("256KB", 262_144),
        ("1MB", 1_048_576),
        ("10MB", 10_485_760),
    ];

    println!("tokio runtime: {runtime}");
    println!("tokio bench scale: {scale:.3}\n");
    if let Some(filter) = &filter {
        println!("tokio bench filter: {filter}\n");
    }

    if mode == "all" || mode == "pingpong" {
        println!("============ Tokio PingPong (RTT latency) ============\n");
        for &(name, size) in sizes {
            if filter
                .as_ref()
                .is_some_and(|filter| !name.to_ascii_lowercase().contains(filter))
            {
                continue;
            }
            let base_count = match size {
                s if s <= 256 => 100_000,
                s if s <= 4_096 => 50_000,
                s if s <= 65_536 => 10_000,
                s if s <= 262_144 => 1_000,
                s if s <= 1_048_576 => 100,
                _ => 20,
            };
            let count = scaled_count(base_count, scale);
            bench_pingpong(name, size, count, (count / 100).max(1)).await;
        }
    }

    if mode == "all" || mode == "burst" {
        println!("============ Tokio Burst (buffering) ============\n");
        for &(name, size) in sizes {
            if filter
                .as_ref()
                .is_some_and(|filter| !name.to_ascii_lowercase().contains(filter))
            {
                continue;
            }
            let (burst_size, base_bursts) = match size {
                s if s <= 256 => (1000, 100),
                s if s <= 4_096 => (500, 50),
                s if s <= 65_536 => (100, 20),
                s if s <= 262_144 => (50, 10),
                s if s <= 1_048_576 => (10, 5),
                _ => (5, 3),
            };
            let bursts = scaled_count(base_bursts, scale);
            bench_burst(name, size, burst_size, bursts).await;
        }
    }

    if mode == "all" || mode == "stream" {
        println!("============ Tokio Streaming (one-way throughput) ============\n");
        for &(name, size) in sizes {
            if filter
                .as_ref()
                .is_some_and(|filter| !name.to_ascii_lowercase().contains(filter))
            {
                continue;
            }
            let base_count = match size {
                s if s <= 256 => 500_000,
                s if s <= 4_096 => 100_000,
                s if s <= 65_536 => 20_000,
                s if s <= 262_144 => 5_000,
                s if s <= 1_048_576 => 1_000,
                _ => 200,
            };
            let count = scaled_count(base_count, scale);
            bench_stream(name, size, count).await;
        }
    }
}

fn main() {
    let runtime = runtime_mode();
    let mut builder = if runtime == "current_thread" {
        tokio::runtime::Builder::new_current_thread()
    } else {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        builder.worker_threads(worker_threads());
        builder
    };

    builder
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run());
}
