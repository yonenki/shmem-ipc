use std::thread;
use std::time::Duration;

use shmem_ipc::{ChannelConfig, ShmemListener, connect};

#[test]
fn test_single_client() {
    let name = "test_ls_single";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    let handle = thread::spawn(move || {
        let mut conn = connect(name, ChannelConfig::default()).unwrap();
        conn.send(b"hello from client").unwrap();
        let reply = conn.recv().unwrap();
        assert_eq!(reply, b"hello from server");
    });

    let mut conn = listener.accept().unwrap();
    let msg = conn.recv().unwrap();
    assert_eq!(msg, b"hello from client");
    conn.send(b"hello from server").unwrap();

    handle.join().unwrap();
}

#[test]
fn test_multiple_clients_sequential() {
    let name = "test_ls_multi_seq";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    for i in 0u32..5 {
        let handle = thread::spawn(move || {
            let mut conn = connect(name, ChannelConfig::default()).unwrap();
            conn.send(&i.to_le_bytes()).unwrap();
            let reply = conn.recv().unwrap();
            assert_eq!(reply, format!("ack_{i}").as_bytes());
        });

        let mut conn = listener.accept().unwrap();
        let msg = conn.recv().unwrap();
        let client_id = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
        assert_eq!(client_id, i);
        conn.send(format!("ack_{client_id}").as_bytes()).unwrap();

        handle.join().unwrap();
    }
}

#[test]
fn test_multiple_clients_concurrent() {
    let name = "test_ls_multi_conc";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    // 3 clients が同時に接続
    let mut handles = Vec::new();
    for i in 0u32..3 {
        let handle = thread::spawn(move || {
            let mut conn = connect(name, ChannelConfig::default()).unwrap();
            conn.send(&i.to_le_bytes()).unwrap();
            let reply = conn.recv().unwrap();
            // ack を受け取れればOK (順序は不定)
            assert_eq!(&reply[..4], b"ack_");
        });
        handles.push(handle);
    }

    // server が 3 接続を順に accept
    for _ in 0..3 {
        let mut conn = listener.accept().unwrap();
        let msg = conn.recv().unwrap();
        let client_id = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
        conn.send(format!("ack_{client_id}").as_bytes()).unwrap();
    }

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_split_connection() {
    let name = "test_ls_split";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    let handle = thread::spawn(move || {
        let conn = connect(name, ChannelConfig::default()).unwrap();
        let (mut tx, mut rx) = conn.split();

        let send_h = thread::spawn(move || {
            for i in 0u32..100 {
                tx.send(&i.to_le_bytes()).unwrap();
            }
        });

        let recv_h = thread::spawn(move || {
            for i in 0u32..100 {
                let msg = rx.recv().unwrap();
                let got = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
                assert_eq!(got, i);
            }
        });

        send_h.join().unwrap();
        recv_h.join().unwrap();
    });

    let conn = listener.accept().unwrap();
    let (mut tx, mut rx) = conn.split();

    let recv_h = thread::spawn(move || {
        for i in 0u32..100 {
            let msg = rx.recv().unwrap();
            let got = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
            assert_eq!(got, i);
        }
    });

    let send_h = thread::spawn(move || {
        for i in 0u32..100 {
            tx.send(&i.to_le_bytes()).unwrap();
        }
    });

    recv_h.join().unwrap();
    send_h.join().unwrap();
    handle.join().unwrap();
}

#[test]
fn test_accept_timeout() {
    let name = "test_ls_timeout";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    let start = std::time::Instant::now();
    let result = listener.accept_timeout(Duration::from_millis(100));
    let elapsed = start.elapsed();

    assert!(matches!(result, Err(shmem_ipc::Error::TimedOut)));
    assert!(elapsed >= Duration::from_millis(90));
    assert!(elapsed < Duration::from_millis(500));
}

#[test]
fn test_accept_timeout_then_multiple_clients_concurrent() {
    let name = "test_ls_timeout_then_multi";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();

    let result = listener.accept_timeout(Duration::from_millis(50));
    assert!(matches!(result, Err(shmem_ipc::Error::TimedOut)));

    let mut handles = Vec::new();
    for i in 0u32..5 {
        handles.push(thread::spawn(move || {
            let mut conn = connect(name, ChannelConfig::default()).unwrap();
            conn.send(&i.to_le_bytes()).unwrap();
            let reply = conn.recv().unwrap();
            assert_eq!(reply, format!("ack_{i}").as_bytes());
        }));
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut accepted = 0usize;
    while accepted < 5 && std::time::Instant::now() < deadline {
        match listener.accept_timeout(Duration::from_millis(200)) {
            Ok(mut conn) => {
                let msg = conn.recv().unwrap();
                let client_id = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
                conn.send(format!("ack_{client_id}").as_bytes()).unwrap();
                accepted += 1;
            }
            Err(shmem_ipc::Error::TimedOut) => {}
            Err(e) => panic!("unexpected accept error: {e:?}"),
        }
    }

    assert_eq!(
        accepted, 5,
        "listener did not accept all concurrent clients"
    );

    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn test_successful_accept_primes_next_instance_before_next_poll() {
    let name = "test_ls_prime_next_accept";
    ShmemListener::cleanup(name);

    let mut listener = ShmemListener::bind(name, ChannelConfig::default()).unwrap();
    let short_connect_config = ChannelConfig {
        connect_timeout: Duration::from_millis(50),
        ..ChannelConfig::default()
    };

    for i in 0u32..3 {
        let config = if i == 0 {
            ChannelConfig::default()
        } else {
            short_connect_config.clone()
        };

        let handle = thread::spawn(move || {
            let mut conn = connect(name, config).unwrap();
            conn.send(&i.to_le_bytes()).unwrap();
            let reply = conn.recv().unwrap();
            assert_eq!(reply, format!("ack_{i}").as_bytes());
        });

        if i > 0 {
            // The next client must be able to connect before the server polls
            // accept again. This is what the Windows textil bench relies on.
            thread::sleep(Duration::from_millis(200));
        }

        let mut conn = listener.accept_timeout(Duration::from_secs(1)).unwrap();
        let msg = conn.recv().unwrap();
        let client_id = u32::from_le_bytes([msg[0], msg[1], msg[2], msg[3]]);
        assert_eq!(client_id, i);
        conn.send(format!("ack_{client_id}").as_bytes()).unwrap();

        handle.join().unwrap();
    }
}
