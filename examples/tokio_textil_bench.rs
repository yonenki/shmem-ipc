use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shmem_ipc::ChannelConfig;
use shmem_ipc::tokio::{ShmemConnection, ShmemListener, connect};

const MSG_REGISTER: u8 = 1;
const MSG_REGISTERED: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_RESPONSE: u8 = 4;
const MSG_DONE: u8 = 255;

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

fn worker_threads() -> usize {
    std::env::var("SHMEM_TOKIO_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .or_else(|| std::thread::available_parallelism().ok().map(usize::from))
        .unwrap_or(4)
}

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

async fn backend_handler(mut conn: ShmemConnection) {
    let msg = conn.recv().await.unwrap();
    assert_eq!(msg_type(&msg), MSG_REGISTER);
    conn.send(&[MSG_REGISTERED]).await.unwrap();

    let mut batch = Vec::with_capacity(64);

    loop {
        batch.clear();
        match conn.recv_many(&mut batch, 64).await {
            Ok(_) => {
                for msg in batch.drain(..) {
                    match msg_type(&msg) {
                        MSG_REQUEST => {
                            let req_id = msg_req_id(&msg);
                            let payload = &msg[5..];
                            conn.send(&encode_response(req_id, payload)).await.unwrap();
                        }
                        MSG_DONE => return,
                        _ => {}
                    }
                }
            }
            Err(_) => break,
        }
    }
}

async fn run_backend(name: String, shutdown: Arc<AtomicBool>, expected_clients: u32) {
    let mut listener = ShmemListener::bind(&name, ChannelConfig::default())
        .await
        .unwrap();
    let mut handles = Vec::new();
    let mut accepted = 0u32;

    while accepted < expected_clients && !shutdown.load(Ordering::Relaxed) {
        match listener.accept_timeout(Duration::from_millis(200)).await {
            Ok(conn) => {
                accepted += 1;
                handles.push(tokio::spawn(backend_handler(conn)));
            }
            Err(shmem_ipc::Error::TimedOut) => {}
            Err(_) => break,
        }
    }

    for handle in handles {
        let _ = handle.await;
    }
}

struct BenchResult {
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

async fn run_frontend_bench(
    name: String,
    frontend_id: u32,
    msg_size: usize,
    count: usize,
) -> BenchResult {
    let mut conn = connect(&name, ChannelConfig::default()).await.unwrap();

    conn.send(&encode_register(&format!("fe_{frontend_id}")))
        .await
        .unwrap();
    let _ = conn.recv().await.unwrap();

    let (mut tx, mut rx) = conn.split();
    let payload = vec![0xABu8; msg_size];

    let recv_future = async {
        let mut received = 0usize;
        let mut total_bytes = 0u64;
        let mut batch = Vec::with_capacity(64);
        while received < count {
            batch.clear();
            let _ = rx
                .recv_many(&mut batch, (count - received).min(64))
                .await
                .unwrap();
            for msg in batch.drain(..) {
                assert_eq!(msg_type(&msg), MSG_RESPONSE);
                total_bytes += msg.len() as u64;
                received += 1;
            }
        }
        total_bytes
    };

    let send_future = async {
        for i in 0..count as u32 {
            tx.send(&encode_request(i, &payload)).await.unwrap();
        }
        tx.send(&[MSG_DONE]).await.unwrap();
    };

    let start = Instant::now();
    let (recv_bytes, ()) = tokio::join!(recv_future, send_future);
    let elapsed = start.elapsed();

    let total_bytes = (5 + msg_size) as u64 * count as u64 + recv_bytes;
    BenchResult {
        messages: count * 2,
        total_bytes,
        elapsed,
    }
}

async fn run_scenario(scenario: &str, num_frontends: u32, msg_size: usize, count: usize) {
    let name = format!("tokio_tbench_{}", scenario.replace(' ', "_"));
    ShmemListener::cleanup(&name);

    let shutdown = Arc::new(AtomicBool::new(false));
    let backend_name = name.clone();
    let backend_shutdown = Arc::clone(&shutdown);
    let backend = tokio::spawn(run_backend(backend_name, backend_shutdown, num_frontends));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut handles = Vec::new();
    for i in 0..num_frontends {
        let frontend_name = name.clone();
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(i as u64 * 10)).await;
            run_frontend_bench(frontend_name, i, msg_size, count).await
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    shutdown.store(true, Ordering::Relaxed);
    let _ = backend.await;

    let total_msgs: usize = results.iter().map(|r| r.messages).sum();
    let total_bytes: u64 = results.iter().map(|r| r.total_bytes).sum();
    let max_elapsed = results.iter().map(|r| r.elapsed).max().unwrap();
    let aggregate_throughput = total_bytes as f64 / (1024.0 * 1024.0) / max_elapsed.as_secs_f64();
    let aggregate_msg_rate = total_msgs as f64 / max_elapsed.as_secs_f64();

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

async fn run() {
    let scale = bench_scale();
    let workers = worker_threads();

    println!("tokio textil pattern benchmark");
    println!("============================\n");
    println!("tokio textil bench scale: {scale:.3}\n");
    println!("tokio worker threads: {workers}\n");

    run_scenario("small-64B", 1, 64, scaled_count(100_000, scale)).await;
    run_scenario("small-64B", 5, 64, scaled_count(100_000, scale)).await;
    run_scenario("small-64B", 10, 64, scaled_count(50_000, scale)).await;

    run_scenario("large-1MB", 1, 1_048_576, scaled_count(100, scale)).await;
    run_scenario("large-1MB", 5, 1_048_576, scaled_count(100, scale)).await;

    run_scenario("medium-4KB", 1, 4_096, scaled_count(50_000, scale)).await;
    run_scenario("medium-4KB", 5, 4_096, scaled_count(50_000, scale)).await;
    run_scenario("medium-4KB", 10, 4_096, scaled_count(20_000, scale)).await;

    println!("  Mixed workload (varying sizes):");
    let name = "tokio_tbench_mixed".to_string();
    ShmemListener::cleanup(&name);

    let shutdown = Arc::new(AtomicBool::new(false));
    let backend_shutdown = Arc::clone(&shutdown);
    let backend_name = name.clone();
    let backend = tokio::spawn(run_backend(backend_name, backend_shutdown, 5));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let sizes = [64, 256, 1024, 4096, 65536, 256 * 1024];
    let start = Instant::now();
    let mut handles = Vec::new();

    for i in 0..5u32 {
        let frontend_name = name.clone();
        handles.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(i as u64 * 10)).await;
            let mut conn = connect(&frontend_name, ChannelConfig::default())
                .await
                .unwrap();
            conn.send(&encode_register(&format!("mix_{i}")))
                .await
                .unwrap();
            let _ = conn.recv().await.unwrap();
            let (mut tx, mut rx) = conn.split();
            let payloads: Vec<Vec<u8>> = sizes.iter().map(|&size| vec![0xCDu8; size]).collect();

            let recv_future = async {
                let mut count = 0usize;
                let expected = scaled_count(600, scale);
                let mut batch = Vec::with_capacity(64);
                while count < expected {
                    batch.clear();
                    let _ = rx
                        .recv_many(&mut batch, (expected - count).min(64))
                        .await
                        .unwrap();
                    count += batch.len();
                }
                count
            };

            let send_future = async {
                for round in 0..scaled_count(100, scale) as u32 {
                    for payload in &payloads {
                        tx.send(&encode_request(round * 100, payload))
                            .await
                            .unwrap();
                    }
                }
                tx.send(&[MSG_DONE]).await.unwrap();
            };

            let (count, ()) = tokio::join!(recv_future, send_future);
            count
        }));
    }

    let mut total = 0usize;
    for handle in handles {
        total += handle.await.unwrap();
    }
    let mixed_elapsed = start.elapsed();
    shutdown.store(true, Ordering::Relaxed);
    let _ = backend.await;

    println!(
        "    5 frontends, 6 sizes x {} rounds = {} msgs each, {total} total recv",
        scaled_count(100, scale),
        scaled_count(600, scale)
    );
    println!("    elapsed: {mixed_elapsed:.2?}");
    println!();

    println!("Done.");
}

fn main() {
    let workers = worker_threads();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run());
}
