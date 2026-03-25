# Tokio Async Design

## Status

This document defines the target design for a real Tokio-native async API for `shmem-ipc`.

It is intentionally stricter than a convenience wrapper:

- no `spawn_blocking` around `send` / `recv` / `accept`
- no hidden polling fallback that silently degrades semantics
- explicit cancellation guarantees
- sync fast path remains the primary implementation and performance baseline

## Implementation Status

The repository now ships a phase-1 Tokio implementation behind the `tokio` feature.

Current implementation:

- ring send/receive paths are split into non-blocking try/peek/commit primitives
- async `send` / `recv` / split halves are cancellation-safe with explicit commit points
- async waits are bridged by dedicated wait threads that translate shared wait objects into Tokio task wakeups
- Unix listener/connect uses Tokio Unix sockets for the control plane
- Windows listener/connect currently uses a dedicated worker thread around the proven sync control plane for robustness

Still target-only in this document:

- Linux `eventfd` + FD-passing wait bridge
- Windows `RegisterWaitForSingleObject` wait bridge
- Windows Tokio named-pipe control plane as the primary implementation

The rest of this document remains the target architecture we still want to converge toward.

## Goals

- Provide a Tokio-facing API that feels native on both Linux and Windows.
- Keep the current sync API and sync performance intact.
- Make `send`, `recv`, `accept`, and split halves cancellation-safe by construction.
- Use the existing shared-memory data plane as the source of truth for progress and correctness.
- Keep OS-specific async waiting behind a narrow internal abstraction.

## Non-goals

- Runtime-agnostic async support in the first iteration.
- `async-std`, `smol`, or generic `futures` integration.
- Replacing the current sync wait path.
- Preserving wire or header compatibility with the current pre-release format.

## External Implementations Studied

### Tokio Windows named pipe

Reference:

- `tokio/src/net/windows/named_pipe.rs`

Useful lessons:

- `WouldBlock -> register interest -> retry` is the correct shape for cancel-safe async I/O.
- Read and write readiness must be treated as separate wait domains.
- Readiness is advisory. The real condition must always be checked again after wake.
- Cancellation safety should be stated explicitly in the API contract.

### interprocess Tokio named pipe listener

Reference:

- `interprocess/src/os/windows/named_pipe/tokio/listener.rs`

Useful lessons:

- Listener `accept` should connect the current instance, pre-create the next instance, and only then hand off the accepted stream.
- Runtime-specific listener types should be real first-class types, not thin adapters over sync listeners.

### kanal async futures

Reference:

- `kanal/src/future.rs`

Useful lessons:

- Async send/recv needs an explicit state machine.
- `Drop` must participate in cancellation semantics.
- Waker replacement and completion races must be handled deliberately.

What we will not copy:

- waiting synchronously in `Drop`
- "put it back into the queue" fallback after a cancelled receive

Those patterns hide ownership ambiguity. `shmem-ipc` should instead make commit points explicit and avoid mutating shared state before the future is ready to return.

## Current Constraints In This Repository

The current implementation has these properties:

- one `Channel` is exactly one server and one client
- each duplex channel contains two SPSC rings
- `RingSender::send` and `RingReceiver::recv` currently combine:
  - fast-path condition checks
  - state mutation
  - blocking wait
- cross-process wait primitives already exist:
  - Linux: futex-backed notify words
  - Windows: generation-scoped named auto-reset events
- `ShmemConnection` / `SendHalf` / `RecvHalf` already encode the natural concurrency model:
  - at most one active sender operation per send half
  - at most one active receiver operation per recv half

That last point is important. It means the async layer only needs one read waiter and one write waiter per endpoint. We do not need an unbounded waiter queue for a single half.

## Chosen Architecture

The async implementation is split into three layers:

1. Non-blocking core operations on top of the existing ring logic.
2. OS-specific async wait bridge.
3. Tokio futures and listener types built on top of 1 and 2.

The sync API continues to use the current blocking wait strategy.

## Public API

The async API is feature-gated and lives in a dedicated module:

```rust
#[cfg(feature = "tokio")]
pub mod tokio;
```

Proposed surface:

```rust
pub mod tokio {
    use crate::{ChannelConfig, Result};
    use std::time::Duration;

    pub struct Channel;
    pub struct ShmemConnection;
    pub struct SendHalf;
    pub struct RecvHalf;
    pub struct ShmemListener;

    pub async fn connect(name: &str, config: ChannelConfig) -> Result<ShmemConnection>;

    impl Channel {
        pub async fn create(name: &str) -> Result<Self>;
        pub async fn create_with_config(name: &str, config: ChannelConfig) -> Result<Self>;
        pub async fn open(name: &str) -> Result<Self>;
        pub async fn open_with_config(name: &str, config: ChannelConfig) -> Result<Self>;

        pub async fn send(&mut self, payload: &[u8]) -> Result<()>;
        pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()>;
        pub async fn recv(&mut self) -> Result<Vec<u8>>;
        pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>>;
        pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>>;

        pub fn into_connection(self) -> ShmemConnection;
    }

    impl ShmemConnection {
        pub async fn send(&mut self, payload: &[u8]) -> Result<()>;
        pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()>;
        pub async fn recv(&mut self) -> Result<Vec<u8>>;
        pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>>;
        pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>>;
        pub fn split(self) -> (SendHalf, RecvHalf);
    }

    impl SendHalf {
        pub async fn send(&mut self, payload: &[u8]) -> Result<()>;
        pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()>;
    }

    impl RecvHalf {
        pub async fn recv(&mut self) -> Result<Vec<u8>>;
        pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize>;
        pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>>;
        pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>>;
    }

    impl ShmemListener {
        pub async fn bind(name: &str, config: ChannelConfig) -> Result<Self>;
        pub async fn accept(&mut self) -> Result<ShmemConnection>;
        pub async fn accept_timeout(&mut self, timeout: Duration) -> Result<ShmemConnection>;
        pub fn cleanup(name: &str);
    }
}
```

## Behavioral Contract

### `send().await`

Guarantees:

- If the future is dropped before it returns `Ready`, no bytes have been published to the ring.
- If it returns `Ok(())`, the payload is fully published and visible to the peer.
- If it returns `Err(Error::ChannelClosed)`, no bytes from that operation were published.

### `recv().await`

Guarantees:

- If the future is dropped before it returns `Ready`, the read cursor has not advanced.
- If it returns `Ok(payload)`, the cursor advance and payload ownership transfer are already committed.
- Buffered data remains drainable after peer close, matching the sync API.

### `accept().await`

Guarantees:

- If cancelled, no accepted client is lost.
- The listener remains usable after a cancelled `accept`.

### Timeouts

- Async timeout methods are implemented with `tokio::time::timeout`.
- Timeout maps to `Error::TimedOut`.
- Timeout does not partially commit send or receive state.

## Core Refactor

The current ring code must be split into non-blocking primitives plus blocking wrappers.

### New non-blocking send API

Add an internal method with no waiting:

```rust
enum TrySendResult {
    Sent,
    Full,
}

impl RingSender {
    fn try_send_once(&mut self, payload: &[u8]) -> Result<TrySendResult>;
}
```

Properties:

- checks `ChannelState`
- checks free space
- if insufficient space, returns `Full` without mutating shared state
- if enough space:
  - writes header + payload
  - advances `write_cursor`
  - bumps `writer.notify`
  - emits async wake signal if configured

The existing sync `send` becomes:

- `try_send_once`
- if `Full`, block using the current wait strategy
- retry

### New non-blocking receive API

Add internal "peek then commit" operations:

```rust
enum TryRecvMeta {
    Empty,
    Ready { payload_len: usize, seq: u32 },
}

impl RingReceiver {
    fn try_peek(&self) -> Result<TryRecvMeta>;
    fn copy_current_message(&self, dst: &mut [u8]) -> Result<usize>;
    fn commit_current_message(&mut self, payload_len: usize, seq: u32) -> Result<()>;
}
```

Properties:

- `try_peek` never advances the cursor
- `copy_current_message` never advances the cursor
- `commit_current_message` is the only method that:
  - validates sequence
  - advances `read_cursor`
  - bumps `reader.notify`
  - emits async wake signal if configured

The existing sync `recv` becomes:

- `try_peek`
- if `Empty`, block using the current wait strategy
- once `Ready`, copy and commit in the same sync call

This refactor is the foundation for cancellation-safe async receive.

## Async Wait Bridge

The data plane remains shared memory.

The async layer only needs a wake mechanism. The wake mechanism is advisory. The notify words and ring cursors remain the source of truth.

### Internal abstraction

```rust
struct AsyncWaitSet {
    ring_a_writer: AsyncWaitHandle,
    ring_a_reader: AsyncWaitHandle,
    ring_b_writer: AsyncWaitHandle,
    ring_b_reader: AsyncWaitHandle,
}

trait AsyncWaitHandle {
    fn current_epoch(&self) -> u64;
    fn poll_changed(&self, cx: &mut Context<'_>, observed: u64) -> Poll<Result<()>>;
}
```

Each wait handle is edge-triggered conceptually but level-safe in behavior:

- if a wake arrives before the future polls, `current_epoch()` already changed
- if multiple wakes coalesce, the epoch still changes and the future re-checks the real ring condition

### Linux design

Linux will use `eventfd` for async wake transport.

Per connection generation:

- create four non-blocking `eventfd`s
  - `ring_a_writer`
  - `ring_a_reader`
  - `ring_b_writer`
  - `ring_b_reader`
- wrap them in `tokio::io::unix::AsyncFd`
- server owns creation
- client receives duplicated FDs from the server over a Unix domain socket using `SCM_RIGHTS`

Why this design:

- writing to `eventfd` is synchronous and cheap
- `eventfd` is pollable by Tokio on Linux
- no helper thread is needed
- wake count coalescing is acceptable because the ring state is the source of truth

Direct `tokio::Channel::create/open` needs a sidecar Unix socket handshake only for FD passing. The shared memory file remains the data plane.

Listener-based async accept can piggyback on the existing Unix socket handshake and send the four `eventfd`s after the connection name.

On emit:

- write `1u64` to the corresponding `eventfd`
- if the `eventfd` is already saturated and returns `EAGAIN`, ignore it
  - this is not a lost wake
  - a saturated `eventfd` is already readable

### Windows design

Windows keeps the existing generation-scoped named auto-reset events as the cross-process signal transport.

Async waiting uses `RegisterWaitForSingleObject` / `UnregisterWaitEx`:

- one registration per logical wait handle
- callback increments an epoch and wakes the stored task waker
- no dedicated blocking helper thread

Why this design:

- `SetEvent` is already the correct synchronous emit primitive
- event registration is native to the platform
- no Tokio-specific I/O shim is required
- no per-operation thread hop

The wait callback never decides correctness. It only says "re-check the ring state now".

## Async State Objects

Each async endpoint owns:

```rust
struct AsyncEndpointState {
    closed: AtomicBool,
    send_epoch: AtomicU64,
    recv_epoch: AtomicU64,
    send_waker: AtomicWaker,
    recv_waker: AtomicWaker,
}
```

Because `SendHalf` and `RecvHalf` require `&mut self` for operations, one waker per direction is sufficient.

## Future State Machines

### Send future

```text
Init
  -> try_send_once == Sent                => Done(Ok)
  -> try_send_once == ChannelClosed       => Done(Err(ChannelClosed))
  -> try_send_once == Full                => Waiting(observed_epoch)

Waiting(observed_epoch)
  -> close observed                       => Done(Err(ChannelClosed))
  -> epoch unchanged                      => Pending
  -> epoch changed                        => Init
```

Implementation notes:

- register `send_waker` before returning `Pending`
- after registering, re-check epoch and channel state once before yielding
- no ring mutation happens while in `Waiting`

### Receive future

```text
Init
  -> try_peek == Ready(meta)              => CopyAndCommit(meta)
  -> try_peek == Empty and open           => Waiting(observed_epoch)
  -> try_peek == Empty and closed         => Done(Err(ChannelClosed))

CopyAndCommit(meta)
  -> copy_current_message
  -> commit_current_message
  -> Done(Ok(payload))

Waiting(observed_epoch)
  -> close observed but buffer not empty  => Init
  -> close observed and still empty       => Done(Err(ChannelClosed))
  -> epoch unchanged                      => Pending
  -> epoch changed                        => Init
```

Implementation notes:

- copy and commit happen in the same poll that returns `Ready`
- dropping the future before that poll leaves the message untouched

### Listener accept future

Unix:

- wait on `tokio::net::UnixListener::accept`
- once the handshake stream is accepted:
  - generate unique `conn_name`
  - create async channel resources
  - send `conn_name`
  - send Linux async bridge FDs via `SCM_RIGHTS`
  - return accepted connection

Windows:

- keep one stored pipe instance
- `connect().await` it
- create the next instance before returning the accepted stream
- run the existing connection-name handshake
- open async channel resources by name

Cancellation rule:

- if cancelled before handoff, the stored listener instance remains valid
- no client is dropped on the floor

## Listener and Direct Handshake

### Async listener

Async listener is a separate implementation. It is not a wrapped sync listener.

Reason:

- sync listener blocks in accept
- async listener has OS-specific handshake duties
- Linux async listener needs FD passing
- Windows async listener needs Tokio named pipe accept semantics

### Direct async create/open

Direct async channel setup differs by OS.

Linux:

- server creates shm file
- server creates eventfds
- server creates a generation-scoped sidecar Unix socket for FD transfer
- client opens shm, reads `wait_key`, connects to the sidecar socket, receives eventfds

Windows:

- server creates shm file
- server creates named wait events as it already does
- client opens shm, reads `wait_key`, opens named wait events

## Internal Module Layout

Proposed layout:

```text
src/
  tokio/
    mod.rs
    channel.rs
    connection.rs
    listener.rs
    future.rs
    bridge/
      mod.rs
      unix.rs
      windows.rs
```

Sync internals stay where they are. Ring internals gain new non-blocking methods but remain shared between sync and async layers.

## Cargo Features

```toml
[features]
default = []
tokio = ["dep:tokio"]

[dependencies.tokio]
version = "1"
optional = true
features = ["net", "rt", "sync", "time", "io-util"]
```

Windows may require additional `windows-sys` symbols for wait registration.

## Error Handling

Public error mapping stays on `crate::Error`:

- timeout -> `Error::TimedOut`
- peer close / local drop observed by async future -> `Error::ChannelClosed`
- bridge setup failure -> `Error::Io`
- malformed payload / sequencing errors -> existing variants

No async-only public error enum is introduced in phase 1.

## Performance Expectations

The async API does not need to beat the sync API.

It does need to meet these constraints:

- sync API benchmarks must not regress
- async ready-fast-path should avoid heap allocation in steady state
- blocked wake should not hop through `spawn_blocking`
- Linux async waits should be eventfd-backed, not timer-polled
- Windows async waits should be event registration-backed, not sleep-polled

Expected relative shape:

- sync remains the fastest option for tight PingPong microbenchmarks
- async should remain competitive for control-plane and service integration workloads
- async throughput should remain dominated by the shared-memory data path, not by wait glue

## Test Plan

### New async tests

- `tokio_channel_basic_send_recv`
- `tokio_channel_split_send_recv`
- `tokio_recv_cancel_does_not_advance_cursor`
- `tokio_send_cancel_does_not_publish`
- `tokio_recv_timeout`
- `tokio_send_timeout`
- `tokio_close_wakes_blocked_recv`
- `tokio_close_wakes_blocked_send`
- `tokio_listener_accept_cancel_is_safe`
- `tokio_listener_multiple_clients`
- `tokio_reconnect_same_name_across_generations`

### Cross-platform assertions

- Linux direct async open/create exercises eventfd sidecar handshake
- Windows direct async open/create exercises named wait-object open path
- listener tests validate no accepted connection loss on cancellation

### Benchmark additions

- async PingPong with `tokio::join!`
- async service-pattern benchmark with many tasks, one connection per client
- sync vs async overhead comparison for:
  - 64B PingPong
  - 4KB PingPong
  - 64KB stream

## Rollout Plan

1. Extract non-blocking ring operations.
2. Refactor sync `send` / `recv` to use those operations without behavior change.
3. Add `tokio` feature and internal async bridge modules.
4. Implement Linux async bridge:
   - eventfd creation
   - sidecar FD passing
   - `AsyncFd` wait handle
5. Implement Windows async bridge:
   - wait registration
   - callback-driven epoch/waker updates
6. Implement async send/recv futures.
7. Implement async direct channel.
8. Implement async listener and `connect`.
9. Add async tests.
10. Add async benchmark examples.

Each step should land with passing sync CI. Async CI can be enabled once Linux and Windows direct channel tests are both green.

## Explicit Rejections

The following designs are rejected for this library:

- `spawn_blocking` wrappers around sync `send` / `recv`
- background polling loops with fixed sleep intervals
- `Drop` implementations that block waiting for an async race to resolve
- receive cancellation that "puts data back" after partial ownership transfer
- a hidden fallback from native async waiting to timer polling

## Summary

The correct Tokio integration for `shmem-ipc` is:

- sync core stays authoritative
- ring operations are split into non-blocking try/commit primitives
- async waiting is implemented with native OS wake objects
- Linux uses `eventfd` plus FD passing
- Windows uses existing named events plus wait registration
- futures are explicitly cancellation-safe because they do not mutate shared state before their commit point

This yields a real async API instead of a disguised blocking wrapper.
