//! Named Pipe (FIFO) で同等のワークロードを実行し比較する
//!
//! ipc-bench の知見を反映:
//! - BufWriter で length + payload を1回の write にまとめる (syscall 削減)
//! - 双方向通信に FIFO 2本使用
//!
//! Usage:
//!   cargo run --release --example named_pipe_bench

#[cfg(unix)]
fn main() {
    use std::fs::{File, OpenOptions};
    use std::io::{BufWriter, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    const MSG_REQUEST: u8 = 3;
    const MSG_RESPONSE: u8 = 4;
    const MSG_DONE: u8 = 255;

    fn fifo_paths(name: &str, id: u32) -> (String, String) {
        (
            format!("/tmp/npbench_{name}_{id}_s2c"),
            format!("/tmp/npbench_{name}_{id}_c2s"),
        )
    }

    fn create_fifo(path: &str) {
        let _ = std::fs::remove_file(path);
        let c_path = std::ffi::CString::new(path).unwrap();
        unsafe { libc::mkfifo(c_path.as_ptr(), 0o644) };
    }

    // BufWriter で length + payload を1回の flush にまとめる (ipc-bench の最適化)
    fn send_msg(w: &mut BufWriter<&mut File>, msg: &[u8]) {
        let len = msg.len() as u32;
        w.write_all(&len.to_le_bytes()).unwrap();
        w.write_all(msg).unwrap();
        w.flush().unwrap();
    }

    fn recv_msg(r: &mut File) -> Vec<u8> {
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

    fn msg_type(msg: &[u8]) -> u8 { msg[0] }
    fn msg_req_id(msg: &[u8]) -> u32 { u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]]) }

    // =========================================================================
    // Backend handler
    // =========================================================================
    fn handle_client(mut reader: File, mut writer: File) {
        // Register (skip)
        let _reg = recv_msg(&mut reader);
        {
            let mut bw = BufWriter::new(&mut writer);
            send_msg(&mut bw, &[2u8]);
        }

        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        let send_h = thread::spawn(move || {
            let mut bw = BufWriter::new(&mut writer);
            while let Ok(msg) = resp_rx.recv() {
                send_msg(&mut bw, &msg);
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
    // Scenario runner
    // =========================================================================
    struct BenchResult {
        messages: usize,
        total_bytes: u64,
        elapsed: Duration,
    }

    fn run_scenario(scenario: &str, num_frontends: u32, msg_size: usize, count: usize) {
        let mut backend_handles = Vec::new();

        // 各 frontend ごとに FIFO ペアを作成し、backend handler を起動
        for i in 0..num_frontends {
            let (s2c, c2s) = fifo_paths(scenario, i);
            create_fifo(&s2c);
            create_fifo(&c2s);

            let s2c_clone = s2c.clone();
            let c2s_clone = c2s.clone();

            backend_handles.push(thread::spawn(move || {
                // Server: write s2c, read c2s
                // デッドロック防止: server は write 側を先に開く
                let writer = OpenOptions::new().write(true).open(&s2c_clone).unwrap();
                let reader = OpenOptions::new().read(true).open(&c2s_clone).unwrap();
                handle_client(reader, writer);
                let _ = std::fs::remove_file(&s2c_clone);
                let _ = std::fs::remove_file(&c2s_clone);
            }));
        }

        // Frontend
        let mut frontend_handles = Vec::new();
        for i in 0..num_frontends {
            let (s2c, c2s) = fifo_paths(scenario, i);

            frontend_handles.push(thread::spawn(move || {
                thread::sleep(Duration::from_millis(i as u64 * 10));

                // Client: read s2c, write c2s
                let mut reader = OpenOptions::new().read(true).open(&s2c).unwrap();
                let mut writer = OpenOptions::new().write(true).open(&c2s).unwrap();

                // Register
                {
                    let mut bw = BufWriter::new(&mut writer);
                    send_msg(&mut bw, &[1u8, b'x']);
                }
                let _ = recv_msg(&mut reader);

                let payload = vec![0xABu8; msg_size];

                // recv thread
                let recv_h = {
                    let mut reader = reader;
                    thread::spawn(move || {
                        let mut total_bytes = 0u64;
                        for _ in 0..count {
                            let msg = recv_msg(&mut reader);
                            total_bytes += msg.len() as u64;
                        }
                        total_bytes
                    })
                };

                let start = Instant::now();
                {
                    let mut bw = BufWriter::new(&mut writer);
                    for i in 0..count as u32 {
                        send_msg(&mut bw, &encode_request(i, &payload));
                    }
                }
                let recv_bytes = recv_h.join().unwrap();
                let elapsed = start.elapsed();

                {
                    let mut bw = BufWriter::new(&mut writer);
                    send_msg(&mut bw, &[MSG_DONE]);
                }

                BenchResult {
                    messages: count * 2,
                    total_bytes: (5 + msg_size) as u64 * count as u64 + recv_bytes,
                    elapsed,
                }
            }));
        }

        let mut results: Vec<BenchResult> =
            frontend_handles.into_iter().map(|h| h.join().unwrap()).collect();
        for h in backend_handles { let _ = h.join(); }

        let total_msgs: usize = results.iter().map(|r| r.messages).sum();
        let total_bytes: u64 = results.iter().map(|r| r.total_bytes).sum();
        let max_elapsed = results.iter().map(|r| r.elapsed).max().unwrap();
        let agg_throughput =
            total_bytes as f64 / (1024.0 * 1024.0) / max_elapsed.as_secs_f64();
        let agg_msg_rate = total_msgs as f64 / max_elapsed.as_secs_f64();

        results.sort_by(|a, b| a.elapsed.cmp(&b.elapsed));
        let median = &results[results.len() / 2];
        let med_msg_rate = median.messages as f64 / median.elapsed.as_secs_f64();
        let med_throughput =
            median.total_bytes as f64 / (1024.0 * 1024.0) / median.elapsed.as_secs_f64();

        println!(
            "  {scenario} ({num_frontends} frontends, {msg_size}B x {count}):"
        );
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
    println!("Named Pipe (FIFO + BufWriter) benchmark (comparison)");
    println!("====================================================\n");

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
    eprintln!("Named Pipe benchmark is only available on Unix platforms");
}
