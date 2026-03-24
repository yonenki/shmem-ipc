# shmem-ipc

`shmem-ipc` is a Rust shared-memory IPC library for local process-to-process communication.

It provides:

- a bidirectional `Channel` built from two lock-free SPSC rings
- a `ShmemListener` / `connect` pair for multi-client accept loops
- blocking, timeout, and try-recv APIs
- Linux futex wait paths and Windows named-event wait paths

The design goal is simple: low latency for small messages, high throughput for bulk transfer, and no hidden fallback that masks bugs.

## Scope

- Local machine only
- One `Channel` is one server and one client
- Multi-client servers are built with `ShmemListener`, which creates one `ShmemConnection` per accepted client
- Header format and internal wait-object layout are still pre-release and may change

## Minimal use

```rust
use shmem_ipc::Channel;

fn main() -> shmem_ipc::Result<()> {
    let mut server = Channel::create("demo")?;

    // In another process:
    // let mut client = Channel::open("demo")?;

    server.send(b"hello")?;
    let reply = server.recv()?;
    assert_eq!(reply, b"world");

    Ok(())
}
```

Listener-based accept loop:

```rust
use shmem_ipc::{ChannelConfig, ShmemListener};

fn main() -> shmem_ipc::Result<()> {
    let mut listener = ShmemListener::bind("demo-service", ChannelConfig::default())?;
    loop {
        let conn = listener.accept()?;
        let (_tx, _rx) = conn.split();
        // hand off to worker threads
    }
}
```

## Validation

The current tree is validated with:

- `cargo test -- --test-threads=1`
- `cargo run --release --example stress`
- `cargo run --release --example bench`
- `cargo run --release --example textil_bench`
- `cargo run --release --example textil_pattern`

## Windows Benchmarks

Representative results from the current implementation on Windows, measured on 2026-03-25 with the repository examples:

| Pattern | Size | Result |
| --- | ---: | ---: |
| PingPong RTT p50 | 64B | 0.30 us |
| PingPong RTT p50 | 4KB | 1.30 us |
| PingPong RTT p50 | 64KB | 25.80 us |
| PingPong RTT p50 | 256KB | 67.40 us |
| PingPong RTT p50 | 1MB | 225.30 us |
| Stream throughput | 64KB | 40.9 GB/s send, 39.2 GB/s recv |
| Stream throughput | 1MB | 33.0 GB/s send, 31.1 GB/s recv |
| Stream throughput | 10MB | 10.8 GB/s send, 10.8 GB/s recv |

The previous Windows wait path had a near-1 ms plateau on larger PingPong sizes. The current event-based wait path removes that plateau and restores low-latency wakeups.

## Windows Comparison

A same-machine comparison was run on Windows against two other local IPC styles:

- Win32 Named Pipe
- TCP loopback

All comparison runs used blocking I/O and fixed-size request/response framing. Numbers below are the observed range across two runs on the same host.

### PingPong RTT p50

| Size | shmem-ipc | Named Pipe | TCP loopback |
| --- | ---: | ---: | ---: |
| 64B | 0.30 us | 10.5 us | 35.6 us |
| 4KB | 0.7-1.1 us | 12.7-12.8 us | 38.0-38.4 us |
| 64KB | 22.8-23.6 us | 15.6-15.9 us | 56.1-61.6 us |
| 1MB | 334.6-415.4 us | 316.2-333.9 us | 559-610 us |

### Streaming Throughput

| Size | shmem-ipc | Named Pipe | TCP loopback |
| --- | ---: | ---: | ---: |
| 64KB | 25.5-26.8 GB/s | 6.6-6.8 GB/s | 2.1 GB/s |
| 1MB | 22.9-25.4 GB/s | 6.4-6.6 GB/s | 2.9-3.1 GB/s |
| 10MB | 8.7-9.0 GB/s | 4.0 GB/s | 1.2-1.3 GB/s |

Practical reading:

- For small messages and one-way bulk transfer, `shmem-ipc` is clearly faster than both Named Pipe and TCP loopback on Windows.
- For large request/response PingPong on Windows, Named Pipe can still be slightly faster in the current implementation.
- For throughput-heavy workloads, shared memory remains the strongest path by a wide margin.

## Notes

- Windows timeout accuracy is implemented with named events plus a high-resolution waitable timer. This avoids the coarse timeout behavior that appears with `WaitForSingleObject(..., 5ms)` alone on some runners.
- Linux uses futex-based waiting and keeps the blocking abstraction zero-cost on that path.
- If you need exact behavior for a target workload, run the examples on the target machine. IPC results are sensitive to message size, scheduler behavior, and CPU topology.
