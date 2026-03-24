//! Unix Socket で同等のワークロードを実行し、shmem-ipc と比較する
//!
//! textil_bench と同じシナリオ・計測方法で、std::os::unix::net のみで実装。
//!
//! Usage:
//!   cargo run --release --example unix_socket_bench

#[cfg(unix)]
fn main() {
    use std::io::{BufWriter, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    const MSG_REQUEST: u8 = 3;
    const MSG_RESPONSE: u8 = 4;
    const MSG_DONE: u8 = 255;

    // length-prefixed framing: [len:u32 LE][payload]
    fn send_msg(w: &mut impl Write, msg: &[u8]) {
        let len = msg.len() as u32;
        w.write_all(&len.to_le_bytes()).unwrap();
        w.write_all(msg).unwrap();
    }

    fn send_msg_flush(w: &mut BufWriter<&mut UnixStream>, msg: &[u8]) {
        let len = msg.len() as u32;
        w.write_all(&len.to_le_bytes()).unwrap();
        w.write_all(msg).unwrap();
        w.flush().unwrap();
    }

    fn recv_msg(r: &mut impl Read) -> Vec<u8> {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).unwrap();
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).unwrap();
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
    // Backend handler (per-connection thread)
    // =========================================================================
    fn handle_client(mut stream: UnixStream) {
        // Register (skip)
        let reg = recv_msg(&mut stream);
        send_msg(&mut stream, &[2u8]); // REGISTERED

        // split: UnixStream は clone で reader/writer を分離できる
        let mut reader = stream.try_clone().unwrap();
        let mut writer = stream;

        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        let send_h = thread::spawn(move || {
            let mut bw = BufWriter::new(&mut writer);
            while let Ok(msg) = resp_rx.recv() {
                send_msg_flush(&mut bw, &msg);
            }
        });

        loop {
            let msg = recv_msg(&mut reader);
            match msg_type(&msg) {
                MSG_REQUEST => {
                    let req_id = msg_req_id(&msg);
                    let payload = &msg[5..];
                    let _ = resp_tx.send(encode_response(req_id, payload));
                }
                MSG_DONE => break,
                _ => break,
            }
        }

        drop(resp_tx);
        let _ = send_h.join();
    }

    // =========================================================================
    // Backend server
    // =========================================================================
    fn run_backend(path: &str, shutdown: Arc<AtomicBool>, expected: u32) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();

        let mut handles = Vec::new();
        let mut accepted = 0u32;

        while accepted < expected && !shutdown.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    accepted += 1;
                    handles.push(thread::spawn(move || handle_client(stream)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break,
            }
        }

        for h in handles {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(path);
    }

    // =========================================================================
    // Frontend benchmark
    // =========================================================================
    struct BenchResult {
        messages: usize,
        total_bytes: u64,
        elapsed: Duration,
    }

    fn run_frontend(path: &str, _id: u32, msg_size: usize, count: usize) -> BenchResult {
        let stream = loop {
            match UnixStream::connect(path) {
                Ok(s) => break s,
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };

        // Register
        send_msg(&mut &stream, &[1u8, b'x']);
        let _ = recv_msg(&mut &stream);

        let mut reader = stream.try_clone().unwrap();
        let mut writer = stream;

        let payload = vec![0xABu8; msg_size];

        let recv_h = thread::spawn(move || {
            let mut total_bytes = 0u64;
            for _ in 0..count {
                let msg = recv_msg(&mut reader);
                total_bytes += msg.len() as u64;
            }
            total_bytes
        });

        let start = Instant::now();
        {
            let mut bw = BufWriter::new(&mut writer);
            for i in 0..count as u32 {
                send_msg_flush(&mut bw, &encode_request(i, &payload));
            }
        }
        let recv_bytes = recv_h.join().unwrap();
        let elapsed = start.elapsed();

        send_msg(&mut writer, &[MSG_DONE]);

        BenchResult {
            messages: count * 2,
            total_bytes: (5 + msg_size) as u64 * count as u64 + recv_bytes,
            elapsed,
        }
    }

    // =========================================================================
    // Scenario runner
    // =========================================================================
    fn run_scenario(scenario: &str, num_frontends: u32, msg_size: usize, count: usize) {
        let path = format!("/tmp/uds_bench_{}.sock", scenario.replace(' ', "_"));
        let _ = std::fs::remove_file(&path);

        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let path_clone = path.clone();

        let backend_h = thread::spawn(move || {
            run_backend(&path_clone, shutdown_clone, num_frontends);
        });

        thread::sleep(Duration::from_millis(50));

        let mut handles = Vec::new();
        for i in 0..num_frontends {
            let path_clone = path.clone();
            handles.push(thread::spawn(move || {
                thread::sleep(Duration::from_millis(i as u64 * 10));
                run_frontend(&path_clone, i, msg_size, count)
            }));
        }

        let mut results: Vec<BenchResult> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        shutdown.store(true, Ordering::Relaxed);
        let _ = backend_h.join();

        let total_msgs: usize = results.iter().map(|r| r.messages).sum();
        let total_bytes: u64 = results.iter().map(|r| r.total_bytes).sum();
        let max_elapsed = results.iter().map(|r| r.elapsed).max().unwrap();
        let agg_throughput = total_bytes as f64 / (1024.0 * 1024.0) / max_elapsed.as_secs_f64();
        let agg_msg_rate = total_msgs as f64 / max_elapsed.as_secs_f64();

        results.sort_by(|a, b| a.elapsed.cmp(&b.elapsed));
        let median = &results[results.len() / 2];
        let med_msg_rate = median.messages as f64 / median.elapsed.as_secs_f64();
        let med_throughput =
            median.total_bytes as f64 / (1024.0 * 1024.0) / median.elapsed.as_secs_f64();

        println!("  {scenario} ({num_frontends} frontends, {msg_size}B x {count}):");
        println!(
            "    per-frontend: {:.2?}, {:.0} msg/s, {:.1} MB/s",
            median.elapsed, med_msg_rate, med_throughput
        );
        println!(
            "    aggregate:    {:.2?}, {:.0} msg/s, {:.1} MB/s",
            max_elapsed, agg_msg_rate, agg_throughput
        );
        println!();
    }

    // =========================================================================
    println!("Unix Socket benchmark (comparison)");
    println!("==================================\n");

    run_scenario("small-64B", 1, 64, 100_000);
    run_scenario("small-64B", 5, 64, 100_000);
    run_scenario("small-64B", 10, 64, 50_000);

    run_scenario("large-1MB", 1, 1_048_576, 100);
    run_scenario("large-1MB", 5, 1_048_576, 100);

    run_scenario("medium-4KB", 1, 4_096, 50_000);
    run_scenario("medium-4KB", 5, 4_096, 50_000);
    run_scenario("medium-4KB", 10, 4_096, 20_000);

    println!("Done.");
}

#[cfg(not(unix))]
fn main() {
    eprintln!("Unix Socket benchmark is only available on Unix platforms");
}
