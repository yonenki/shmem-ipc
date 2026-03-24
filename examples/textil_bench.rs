//! textil パターンのパフォーマンスベンチマーク
//!
//! 1 Backend + N Frontend で、実際的なワークロードをハードに回して実測する。
//!
//! シナリオ:
//! 1. Request/Response 高頻度 (64B payload x 10K per frontend)
//! 2. Request/Response 大容量 (1MB payload x 100 per frontend)
//! 3. Streaming 大量チャンク (4KB x 1000 chunks per frontend)
//! 4. 混合ワークロード (小/中/大が混在)
//!
//! Usage:
//!   cargo run --release --example textil_bench

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{ChannelConfig, ShmemConnection, ShmemListener, connect};

// メッセージタイプ
const MSG_REGISTER: u8 = 1;
const MSG_REGISTERED: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_RESPONSE: u8 = 4;
const MSG_DONE: u8 = 255;

fn encode_register(id: &str) -> Vec<u8> {
    let mut buf = vec![MSG_REGISTER];
    buf.extend_from_slice(id.as_bytes());
    buf
}

fn encode_request(req_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(MSG_REQUEST);
    buf.extend_from_slice(&req_id.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn encode_response(req_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + payload.len());
    buf.push(MSG_RESPONSE);
    buf.extend_from_slice(&req_id.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn msg_type(msg: &[u8]) -> u8 {
    msg[0]
}
fn msg_req_id(msg: &[u8]) -> u32 {
    u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]])
}

// =========================================================================
// Backend: echo server (split して recv → process → send)
// =========================================================================

fn backend_handler(conn: ShmemConnection) {
    let (mut tx, mut rx) = conn.split();

    // Register
    let msg = rx.recv().unwrap();
    assert_eq!(msg_type(&msg), MSG_REGISTER);
    let id = String::from_utf8_lossy(&msg[1..]).to_string();
    tx.send(&[MSG_REGISTERED]).unwrap();

    // mpsc で recv → send を分離
    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    let send_h = thread::spawn(move || {
        while let Ok(msg) = resp_rx.recv() {
            if tx.send(&msg).is_err() {
                break;
            }
        }
    });

    loop {
        match rx.recv() {
            Ok(msg) => {
                match msg_type(&msg) {
                    MSG_REQUEST => {
                        let req_id = msg_req_id(&msg);
                        let payload = &msg[5..];
                        // Echo back
                        let _ = resp_tx.send(encode_response(req_id, payload));
                    }
                    MSG_DONE => break,
                    _ => {}
                }
            }
            Err(_) => break,
        }
    }

    drop(resp_tx);
    let _ = send_h.join();
}

fn run_backend(name: &str, shutdown: Arc<AtomicBool>, expected_clients: u32) {
    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();
    let mut handles = Vec::new();
    let mut accepted = 0u32;

    while accepted < expected_clients && !shutdown.load(Ordering::Relaxed) {
        match listener.accept_timeout(Duration::from_millis(200)) {
            Ok(conn) => {
                accepted += 1;
                handles.push(thread::spawn(move || backend_handler(conn)));
            }
            Err(shmem_ipc::Error::TimedOut) => continue,
            Err(_) => break,
        }
    }

    for h in handles {
        let _ = h.join();
    }
}

// =========================================================================
// Frontend: ベンチマーク実行
// =========================================================================

struct BenchResult {
    scenario: String,
    messages: usize,
    total_bytes: u64,
    elapsed: Duration,
}

impl BenchResult {
    fn throughput_mbs(&self) -> f64 {
        self.total_bytes as f64 / (1024.0 * 1024.0) / self.elapsed.as_secs_f64()
    }
    fn msg_per_sec(&self) -> f64 {
        self.messages as f64 / self.elapsed.as_secs_f64()
    }
}

fn run_frontend_bench(
    name: &str,
    frontend_id: u32,
    scenario: &str,
    msg_size: usize,
    count: usize,
) -> BenchResult {
    let mut conn = connect(name, ChannelConfig::default()).unwrap();

    // Register
    conn.send(&encode_register(&format!("fe_{frontend_id}")))
        .unwrap();
    let _ = conn.recv().unwrap();

    let (mut tx, mut rx) = conn.split();

    let payload = vec![0xABu8; msg_size];
    let count_clone = count;

    // Recv スレッド
    let recv_h = thread::spawn(move || {
        let mut received = 0usize;
        let mut total_bytes = 0u64;
        while received < count_clone {
            let msg = rx.recv().unwrap();
            assert_eq!(msg_type(&msg), MSG_RESPONSE);
            total_bytes += msg.len() as u64;
            received += 1;
        }
        total_bytes
    });

    // Send + 計測
    let start = Instant::now();
    for i in 0..count as u32 {
        tx.send(&encode_request(i, &payload)).unwrap();
    }
    let recv_bytes = recv_h.join().unwrap();
    let elapsed = start.elapsed();

    // Done signal
    tx.send(&[MSG_DONE]).unwrap();

    let total_bytes = (5 + msg_size) as u64 * count as u64 + recv_bytes;

    BenchResult {
        scenario: scenario.to_string(),
        messages: count * 2, // send + recv
        total_bytes,
        elapsed,
    }
}

// =========================================================================
// Main
// =========================================================================

fn run_scenario(scenario: &str, num_frontends: u32, msg_size: usize, count: usize) {
    let name = &format!("tbench_{}", scenario.replace(' ', "_"));
    ShmemListener::cleanup(name);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    let name_owned = name.to_string();
    let backend_h = thread::spawn(move || {
        run_backend(&name_owned, shutdown_clone, num_frontends);
    });

    thread::sleep(Duration::from_millis(50));

    let mut handles = Vec::new();
    for i in 0..num_frontends {
        let name_owned = name.to_string();
        let scenario_owned = scenario.to_string();
        handles.push(thread::spawn(move || {
            thread::sleep(Duration::from_millis(i as u64 * 10));
            run_frontend_bench(&name_owned, i, &scenario_owned, msg_size, count)
        }));
    }

    let mut results: Vec<BenchResult> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    shutdown.store(true, Ordering::Relaxed);
    let _ = backend_h.join();

    // Aggregate
    let total_msgs: usize = results.iter().map(|r| r.messages).sum();
    let total_bytes: u64 = results.iter().map(|r| r.total_bytes).sum();
    let max_elapsed = results.iter().map(|r| r.elapsed).max().unwrap();
    let aggregate_throughput = total_bytes as f64 / (1024.0 * 1024.0) / max_elapsed.as_secs_f64();
    let aggregate_msg_rate = total_msgs as f64 / max_elapsed.as_secs_f64();

    // Per-frontend median
    results.sort_by(|a, b| a.elapsed.cmp(&b.elapsed));
    let median = &results[results.len() / 2];

    println!("  {scenario} ({num_frontends} frontends, {msg_size}B x {count}):");
    println!(
        "    per-frontend: {:.2?}, {:.0} msg/s, {:.1} MB/s",
        median.elapsed,
        median.msg_per_sec(),
        median.throughput_mbs()
    );
    println!(
        "    aggregate:    {:.2?}, {:.0} msg/s, {:.1} MB/s",
        max_elapsed, aggregate_msg_rate, aggregate_throughput
    );
    println!();
}

fn main() {
    println!("textil pattern benchmark");
    println!("========================\n");

    // Scenario 1: 高頻度小メッセージ
    run_scenario("small-64B", 1, 64, 100_000);
    run_scenario("small-64B", 5, 64, 100_000);
    run_scenario("small-64B", 10, 64, 50_000);

    // Scenario 2: 大容量メッセージ
    run_scenario("large-1MB", 1, 1_048_576, 100);
    run_scenario("large-1MB", 5, 1_048_576, 100);

    // Scenario 3: 中サイズ高頻度
    run_scenario("medium-4KB", 1, 4_096, 50_000);
    run_scenario("medium-4KB", 5, 4_096, 50_000);
    run_scenario("medium-4KB", 10, 4_096, 20_000);

    // Scenario 4: 混合 (小中大の組み合わせ)
    println!("  Mixed workload (varying sizes):");
    let name = "tbench_mixed";
    ShmemListener::cleanup(name);
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    let backend_h = thread::spawn(move || run_backend(name, shutdown_clone, 5));
    thread::sleep(Duration::from_millis(50));

    let sizes = [64, 256, 1024, 4096, 65536, 256 * 1024];
    let start = Instant::now();
    let mut handles = Vec::new();
    for i in 0..5u32 {
        handles.push(thread::spawn(move || {
            thread::sleep(Duration::from_millis(i as u64 * 10));
            let mut conn = connect(name, ChannelConfig::default()).unwrap();
            conn.send(&encode_register(&format!("mix_{i}"))).unwrap();
            let _ = conn.recv().unwrap();
            let (mut tx, mut rx) = conn.split();

            let recv_h = thread::spawn(move || {
                let mut count = 0;
                for _ in 0..600 {
                    // 6 sizes * 100 rounds
                    let _ = rx.recv().unwrap();
                    count += 1;
                }
                count
            });

            for round in 0..100u32 {
                for &size in &sizes {
                    let payload = vec![0xCDu8; size];
                    tx.send(&encode_request(round * 100, &payload)).unwrap();
                }
            }

            let count = recv_h.join().unwrap();
            tx.send(&[MSG_DONE]).unwrap();
            count
        }));
    }

    let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let mixed_elapsed = start.elapsed();
    shutdown.store(true, Ordering::Relaxed);
    let _ = backend_h.join();

    println!("    5 frontends, 6 sizes x 100 rounds = 3000 msgs each, {total} total recv");
    println!("    elapsed: {mixed_elapsed:.2?}");
    println!();

    println!("Done.");
}
