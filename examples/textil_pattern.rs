//! textil パターンのリアリスティックなストレステスト
//!
//! 1 Backend + N Frontend のアーキテクチャで、textil の実際の通信パターンを再現:
//! - Register (接続直後の認証)
//! - Request/Response (request_id による相関)
//! - Streaming (StreamStart → StreamData × N → StreamEnd)
//! - Pub/Sub (Watch → 非同期 Event push)
//! - 各 Frontend が split して送受信を並行
//! - Frontend が途中で切断
//! - Backend が全 Frontend を管理
//!
//! Usage:
//!   cargo run --release --example textil_pattern

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use shmem_ipc::{ChannelConfig, ShmemConnection, ShmemListener, connect};

// =========================================================================
// メッセージ定義 (textil の ClientMessage/ServerMessage を簡略化)
// =========================================================================

// メッセージタイプ (先頭 1byte)
const MSG_REGISTER: u8 = 1;
const MSG_REGISTERED: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_RESPONSE: u8 = 4;
const MSG_STREAM_START: u8 = 5;
const MSG_STREAM_DATA: u8 = 6;
const MSG_STREAM_END: u8 = 7;
const MSG_WATCH: u8 = 8;
const MSG_EVENT: u8 = 9;

fn encode_register(instance_id: &str) -> Vec<u8> {
    let mut buf = vec![MSG_REGISTER];
    buf.extend_from_slice(instance_id.as_bytes());
    buf
}

fn encode_registered(instance_id: &str) -> Vec<u8> {
    let mut buf = vec![MSG_REGISTERED];
    buf.extend_from_slice(instance_id.as_bytes());
    buf
}

fn encode_request(request_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![MSG_REQUEST];
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn encode_response(request_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![MSG_RESPONSE];
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

fn encode_stream_start(request_id: u32, total: u32) -> Vec<u8> {
    let mut buf = vec![MSG_STREAM_START];
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&total.to_le_bytes());
    buf
}

fn encode_stream_data(request_id: u32, chunk: &[u8]) -> Vec<u8> {
    let mut buf = vec![MSG_STREAM_DATA];
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(chunk);
    buf
}

fn encode_stream_end(request_id: u32) -> Vec<u8> {
    let mut buf = vec![MSG_STREAM_END];
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf
}

fn encode_watch(topic: &str) -> Vec<u8> {
    let mut buf = vec![MSG_WATCH];
    buf.extend_from_slice(topic.as_bytes());
    buf
}

fn encode_event(topic: &str, data: &[u8]) -> Vec<u8> {
    let mut buf = vec![MSG_EVENT];
    let topic_bytes = topic.as_bytes();
    buf.push(topic_bytes.len() as u8);
    buf.extend_from_slice(topic_bytes);
    buf.extend_from_slice(data);
    buf
}

fn msg_type(msg: &[u8]) -> u8 {
    msg[0]
}

fn msg_request_id(msg: &[u8]) -> u32 {
    u32::from_le_bytes([msg[1], msg[2], msg[3], msg[4]])
}

// =========================================================================
// Backend
// =========================================================================

fn run_backend(name: &str, shutdown: Arc<AtomicBool>, connected_count: Arc<AtomicU32>) {
    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();
    println!("[backend] listening on '{name}'");

    let mut client_threads: Vec<thread::JoinHandle<()>> = Vec::new();

    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept_timeout(Duration::from_millis(100)) {
            Ok(conn) => {
                let count = connected_count.fetch_add(1, Ordering::Relaxed) + 1;
                println!("[backend] accepted connection #{count}");

                let shutdown_clone = shutdown.clone();
                let handle = thread::spawn(move || {
                    handle_client(conn, shutdown_clone);
                });
                client_threads.push(handle);
            }
            Err(shmem_ipc::Error::TimedOut) => continue,
            Err(e) => {
                if !shutdown.load(Ordering::Relaxed) {
                    eprintln!("[backend] accept error: {e}");
                }
                break;
            }
        }
    }

    for h in client_threads {
        let _ = h.join();
    }
    println!("[backend] shutdown");
}

fn handle_client(conn: ShmemConnection, shutdown: Arc<AtomicBool>) {
    let (mut tx, mut rx) = conn.split();

    // 1. Register (split 前に unsplit conn で行ってもよいが、既に split 済みなので rx/tx で)
    let instance_id = match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(msg) if msg_type(&msg) == MSG_REGISTER => {
            let id = String::from_utf8_lossy(&msg[1..]).to_string();
            tx.send(&encode_registered(&id)).unwrap();
            println!("[backend] registered: {id}");
            id
        }
        Ok(msg) => {
            eprintln!("[backend] unexpected msg type: {}", msg_type(&msg));
            return;
        }
        Err(e) => {
            eprintln!("[backend] register timeout: {e}");
            return;
        }
    };

    // 2. レスポンス送信用の channel (スレッド間)
    let (resp_tx, resp_rx) = std::sync::mpsc::channel::<Vec<u8>>();

    // 3. 送信スレッド: resp_rx からレスポンスを読んで tx に書く
    let instance_id_clone = instance_id.clone();
    let shutdown_clone = shutdown.clone();
    let send_handle = thread::spawn(move || {
        while let Ok(msg) = resp_rx.recv() {
            if tx.send(&msg).is_err() {
                break;
            }
        }
    });

    // 4. 受信ループ: rx からリクエストを読んで処理、レスポンスを resp_tx に送る
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        let msg = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(msg) => msg,
            Err(shmem_ipc::Error::TimedOut) => continue,
            Err(shmem_ipc::Error::ChannelClosed) => {
                println!("[backend] client {instance_id} disconnected");
                break;
            }
            Err(e) => {
                eprintln!("[backend] recv error for {instance_id}: {e}");
                break;
            }
        };

        match msg_type(&msg) {
            MSG_REQUEST => {
                let req_id = msg_request_id(&msg);
                let payload = &msg[5..];
                let _ = resp_tx.send(encode_response(req_id, payload));
            }

            MSG_WATCH => {
                let topic = String::from_utf8_lossy(&msg[1..]).to_string();
                println!("[backend] {instance_id} watching '{topic}'");
                // Event を数回 push
                for _ in 0..3 {
                    let event_data = format!("event for {instance_id}");
                    let _ = resp_tx.send(encode_event(&topic, event_data.as_bytes()));
                }
            }

            MSG_STREAM_START => {
                let req_id = msg_request_id(&msg);
                let chunk_count = 10u32;
                let chunk = vec![0xABu8; 4096];

                let _ = resp_tx.send(encode_stream_start(req_id, chunk_count));
                for _ in 0..chunk_count {
                    let _ = resp_tx.send(encode_stream_data(req_id, &chunk));
                }
                let _ = resp_tx.send(encode_stream_end(req_id));
            }

            _ => {
                eprintln!("[backend] unknown msg type: {}", msg_type(&msg));
            }
        }
    }

    drop(resp_tx); // send スレッドを終了させる
    let _ = send_handle.join();
}

// =========================================================================
// Frontend
// =========================================================================

fn run_frontend(name: &str, frontend_id: u32, results: Arc<std::sync::Mutex<Vec<String>>>) {
    let instance_id = format!("frontend_{frontend_id}");
    let mut conn = connect(name, ChannelConfig::default()).unwrap();

    // 1. Register
    conn.send(&encode_register(&instance_id)).unwrap();
    let reply = conn.recv().unwrap();
    assert_eq!(msg_type(&reply), MSG_REGISTERED);
    println!("[{instance_id}] registered");

    // 2. Split for concurrent send/recv
    let (mut tx, mut rx) = conn.split();

    let instance_id_clone = instance_id.clone();
    let results_clone = results.clone();

    // Receiver thread: 全レスポンスを収集
    let recv_handle = thread::spawn(move || {
        let mut responses: HashMap<u32, Vec<u8>> = HashMap::new();
        let mut stream_chunks: HashMap<u32, Vec<Vec<u8>>> = HashMap::new();
        let mut events: Vec<Vec<u8>> = Vec::new();
        let mut expected_responses = 0u32;
        let mut got_responses = 0u32;
        let mut stream_complete = false;

        loop {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(msg) => match msg_type(&msg) {
                    MSG_RESPONSE => {
                        let req_id = msg_request_id(&msg);
                        responses.insert(req_id, msg[5..].to_vec());
                        got_responses += 1;
                    }
                    MSG_STREAM_START => {
                        let req_id = msg_request_id(&msg);
                        stream_chunks.insert(req_id, Vec::new());
                    }
                    MSG_STREAM_DATA => {
                        let req_id = msg_request_id(&msg);
                        if let Some(chunks) = stream_chunks.get_mut(&req_id) {
                            chunks.push(msg[5..].to_vec());
                        }
                    }
                    MSG_STREAM_END => {
                        stream_complete = true;
                    }
                    MSG_EVENT => {
                        events.push(msg);
                    }
                    _ => {}
                },
                Err(shmem_ipc::Error::TimedOut) => {
                    // タイムアウト — 全レスポンスを受信したか確認
                    if got_responses >= 50 && stream_complete {
                        break;
                    }
                }
                Err(shmem_ipc::Error::ChannelClosed) => break,
                Err(_) => break,
            }
        }

        let mut result_msgs = Vec::new();
        result_msgs.push(format!(
            "{instance_id_clone}: {got_responses} responses, {} stream chunks, {} events",
            stream_chunks.values().map(|c| c.len()).sum::<usize>(),
            events.len()
        ));

        // 検証
        assert!(
            got_responses >= 50,
            "{instance_id_clone}: expected >=50 responses, got {got_responses}"
        );
        assert!(stream_complete, "{instance_id_clone}: stream not completed");
        for (req_id, data) in &responses {
            // Echo response の検証
            let expected = format!("request_{req_id}");
            assert_eq!(
                data,
                expected.as_bytes(),
                "{instance_id_clone}: response mismatch for req {req_id}"
            );
        }

        results_clone.lock().unwrap().extend(result_msgs);
    });

    // Sender thread: リクエストを送信
    let send_handle = thread::spawn(move || {
        // Watch を登録
        tx.send(&encode_watch("repo/main")).unwrap();

        // 50 個の Request/Response
        for i in 0u32..50 {
            let payload = format!("request_{i}");
            tx.send(&encode_request(i, payload.as_bytes())).unwrap();
            // 少し間を空ける (リアルなパターン)
            if i % 10 == 0 {
                thread::sleep(Duration::from_millis(1));
            }
        }

        // Streaming request
        tx.send(&encode_stream_start(1000, 0)).unwrap();

        // 送信完了後しばらく待つ (Event を受信するため)
        thread::sleep(Duration::from_millis(200));
    });

    send_handle.join().unwrap();
    recv_handle.join().unwrap();
    println!("[{instance_id}] done");
}

// =========================================================================
// main
// =========================================================================

fn main() {
    let name = "textil_test";
    ShmemListener::cleanup(name);

    let num_frontends = 5;
    let shutdown = Arc::new(AtomicBool::new(false));
    let connected_count = Arc::new(AtomicU32::new(0));
    let results = Arc::new(std::sync::Mutex::new(Vec::new()));

    println!("=== textil pattern test: 1 Backend + {num_frontends} Frontends ===\n");
    let start = Instant::now();

    // Backend
    let shutdown_clone = shutdown.clone();
    let connected_clone = connected_count.clone();
    let backend_handle = thread::spawn(move || {
        run_backend(name, shutdown_clone, connected_clone);
    });

    // 少し待ってから Frontend を起動
    thread::sleep(Duration::from_millis(100));

    // Frontends (並行に起動)
    let mut frontend_handles = Vec::new();
    for i in 0..num_frontends {
        let results_clone = results.clone();
        let handle = thread::spawn(move || {
            // 少しずらして接続 (同時接続のテスト)
            thread::sleep(Duration::from_millis(i as u64 * 20));
            run_frontend(name, i, results_clone);
        });
        frontend_handles.push(handle);
    }

    // Frontend 完了を待つ
    for h in frontend_handles {
        h.join().unwrap();
    }

    // Backend shutdown
    shutdown.store(true, Ordering::Relaxed);
    // Backend の accept_timeout が抜けるのを待つ
    thread::sleep(Duration::from_millis(200));
    drop(backend_handle.join());

    let elapsed = start.elapsed();

    println!("\n=== Results ===");
    let results = results.lock().unwrap();
    for r in results.iter() {
        println!("  {r}");
    }
    assert_eq!(
        connected_count.load(Ordering::Relaxed),
        num_frontends,
        "not all frontends connected"
    );
    println!("\nAll {num_frontends} frontends completed in {elapsed:.2?}");
    println!("PASS");
}
