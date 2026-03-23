//! shmem-ipc ベンチマーク
//!
//! ipc-bench と同条件で計測し、製品版の SpinThenWait による性能影響を確認する。
//!
//! Usage:
//!   cargo run --release --example bench

use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{Channel, ChannelConfig, SpinThenWait};

struct Scenario {
    name: &'static str,
    msg_size: usize,
    count: usize,
    rounds: usize,
    warmup: usize,
}

fn run_scenario(scenario: &Scenario) {
    let channel_name = format!("bench_{}", scenario.name);
    let _ = Channel::cleanup(&channel_name);

    let config = ChannelConfig {
        wait_strategy: SpinThenWait { spin_count: 256 },
        ..Default::default()
    };

    let mut server = Channel::create_with_config(&channel_name, config).unwrap();

    let msg_size = scenario.msg_size;
    let count = scenario.count;
    let rounds = scenario.rounds;
    let warmup = scenario.warmup;
    let name_clone = channel_name.clone();

    let handle = thread::spawn(move || {
        let config = ChannelConfig {
            wait_strategy: SpinThenWait { spin_count: 256 },
            ..Default::default()
        };
        let mut client = Channel::open_with_config(&name_clone, config).unwrap();
        let mut buf = vec![0u8; msg_size];

        // warmup + rounds * count 回エコー
        for _ in 0..(warmup + rounds * count) {
            let n = client.recv_into(&mut buf).unwrap();
            client.send(&buf[..n]).unwrap();
        }
    });

    let send_buf = vec![0xABu8; msg_size];
    let mut recv_buf = vec![0u8; msg_size];

    // warmup
    for _ in 0..warmup {
        server.send(&send_buf).unwrap();
        server.recv_into(&mut recv_buf).unwrap();
    }

    // rounds
    let mut round_throughputs: Vec<f64> = Vec::new();
    let mut round_p50s: Vec<Duration> = Vec::new();

    for r in 0..rounds {
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
        let total_bytes = msg_size as u64 * count as u64 * 2;
        let throughput = total_bytes as f64 / (1024.0 * 1024.0) / total.as_secs_f64();

        println!(
            "  Round {}/{}: p50={:.2?}, total={:.2?}, {:.1} MB/s",
            r + 1,
            rounds,
            p50,
            total,
            throughput
        );
        round_throughputs.push(throughput);
        round_p50s.push(p50);
    }

    handle.join().unwrap();

    // 中央値を選択
    round_throughputs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    round_p50s.sort();
    let median_throughput = round_throughputs[rounds / 2];
    let median_p50 = round_p50s[rounds / 2];

    println!(
        "=== {} ({}B x {}) === p50={:.2?}, {:.1} MB/s",
        scenario.name, msg_size, count, median_p50, median_throughput
    );
    println!();
}

fn main() {
    let scenarios = vec![
        Scenario {
            name: "small-64B",
            msg_size: 64,
            count: 100_000,
            rounds: 5,
            warmup: 1000,
        },
        Scenario {
            name: "large-1MB",
            msg_size: 1_048_576,
            count: 100,
            rounds: 5,
            warmup: 10,
        },
        Scenario {
            name: "vlarge-10MB",
            msg_size: 10_485_760,
            count: 20,
            rounds: 5,
            warmup: 5,
        },
    ];

    println!("shmem-ipc benchmark (SpinThenWait, 5 rounds median)");
    println!("====================================================");
    println!();

    for s in &scenarios {
        println!("--- {} ({}B x {}, warmup={}) ---", s.name, s.msg_size, s.count, s.warmup);
        run_scenario(s);
    }
}
