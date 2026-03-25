mod bridge;
mod listener;

use std::hint::spin_loop;
use std::time::Duration;

use bridge::AsyncWait;

use crate::channel::ChannelConfig;
use crate::connection::{
    RecvHalf as SyncRecvHalf, SendHalf as SyncSendHalf, ShmemConnection as SyncConnection,
};
use crate::error::{Error, Result};
use crate::ring::{PeekedMessage, RingReceiver, RingSender, TryPeekResult, TrySendResult};

pub use listener::{ShmemListener, connect};

pub struct Channel {
    send_wait: Option<AsyncWait>,
    recv_wait: Option<AsyncWait>,
    inner: Option<crate::Channel>,
}

impl Channel {
    pub async fn create(name: &str) -> Result<Self> {
        Self::create_with_config(name, ChannelConfig::default()).await
    }

    pub async fn create_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        Ok(Self::from_sync(crate::Channel::create_with_config(
            name, config,
        )?))
    }

    pub async fn open(name: &str) -> Result<Self> {
        Self::open_with_config(name, ChannelConfig::default()).await
    }

    pub async fn open_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        wait_for_channel_ready(name, &config).await?;
        Ok(Self::from_sync(crate::Channel::open_with_config(
            name, config,
        )?))
    }

    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        let waiter = self.send_wait.as_mut().expect("send waiter missing");
        let inner = self.inner.as_mut().expect("channel already moved");
        send_async(inner.sender_mut(), waiter, payload).await
    }

    pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        map_timeout(::tokio::time::timeout(timeout, self.send(payload))).await
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let waiter = self.recv_wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("channel already moved");
        recv_async(inner.receiver_mut(), waiter).await
    }

    pub async fn recv_many(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        max_messages: usize,
    ) -> Result<usize> {
        let waiter = self.recv_wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("channel already moved");
        recv_many_async(inner.receiver_mut(), waiter, out, max_messages).await
    }

    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        map_timeout(::tokio::time::timeout(timeout, self.recv())).await
    }

    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        let waiter = self.recv_wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("channel already moved");
        recv_into_async(inner.receiver_mut(), waiter, buf).await
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.inner
            .as_mut()
            .expect("channel already moved")
            .try_recv()
    }

    pub fn drain_ready(&mut self, out: &mut Vec<Vec<u8>>, max_messages: usize) -> Result<usize> {
        let inner = self.inner.as_mut().expect("channel already moved");
        drain_ready(inner.receiver_mut(), out, max_messages)
    }

    pub fn into_connection(mut self) -> ShmemConnection {
        self.send_wait.take();
        self.recv_wait.take();
        let inner = self.inner.take().expect("channel already moved");
        ShmemConnection::from_sync(inner.into_connection())
    }

    fn from_sync(inner: crate::Channel) -> Self {
        let spin_count = inner.wait_spin_count();
        let send_wait = AsyncWait::new(inner.send_wait_target(), spin_count);
        let recv_wait = AsyncWait::new(inner.recv_wait_target(), spin_count);
        Self {
            send_wait: Some(send_wait),
            recv_wait: Some(recv_wait),
            inner: Some(inner),
        }
    }
}

pub struct ShmemConnection {
    send_wait: Option<AsyncWait>,
    recv_wait: Option<AsyncWait>,
    inner: Option<SyncConnection>,
}

impl ShmemConnection {
    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        let waiter = self.send_wait.as_mut().expect("send waiter missing");
        let inner = self.inner.as_mut().expect("connection already moved");
        send_async(inner.sender_mut(), waiter, payload).await
    }

    pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        map_timeout(::tokio::time::timeout(timeout, self.send(payload))).await
    }

    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let waiter = self.recv_wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("connection already moved");
        recv_async(inner.receiver_mut(), waiter).await
    }

    pub async fn recv_many(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        max_messages: usize,
    ) -> Result<usize> {
        let waiter = self.recv_wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("connection already moved");
        recv_many_async(inner.receiver_mut(), waiter, out, max_messages).await
    }

    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        map_timeout(::tokio::time::timeout(timeout, self.recv())).await
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.inner
            .as_mut()
            .expect("connection already moved")
            .try_recv()
    }

    pub fn drain_ready(&mut self, out: &mut Vec<Vec<u8>>, max_messages: usize) -> Result<usize> {
        let inner = self.inner.as_mut().expect("connection already moved");
        drain_ready(inner.receiver_mut(), out, max_messages)
    }

    pub fn split(mut self) -> (SendHalf, RecvHalf) {
        self.send_wait.take();
        self.recv_wait.take();
        let inner = self.inner.take().expect("connection already moved");
        let (sender, receiver) = inner.split();
        (SendHalf::from_sync(sender), RecvHalf::from_sync(receiver))
    }

    pub fn name(&self) -> &str {
        self.inner
            .as_ref()
            .expect("connection already moved")
            .name()
    }

    pub(crate) fn from_sync(inner: SyncConnection) -> Self {
        let spin_count = inner.wait_spin_count();
        let send_wait = AsyncWait::new(inner.send_wait_target(), spin_count);
        let recv_wait = AsyncWait::new(inner.recv_wait_target(), spin_count);
        Self {
            send_wait: Some(send_wait),
            recv_wait: Some(recv_wait),
            inner: Some(inner),
        }
    }
}

pub struct SendHalf {
    wait: Option<AsyncWait>,
    inner: Option<SyncSendHalf>,
}

impl SendHalf {
    pub async fn send(&mut self, payload: &[u8]) -> Result<()> {
        let wait = self.wait.as_mut().expect("send waiter missing");
        let inner = self.inner.as_mut().expect("send half already moved");
        send_async(inner.sender_mut(), wait, payload).await
    }

    pub async fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        map_timeout(::tokio::time::timeout(timeout, self.send(payload))).await
    }

    fn from_sync(inner: SyncSendHalf) -> Self {
        let wait = AsyncWait::new(inner.send_wait_target(), inner.wait_spin_count());
        Self {
            wait: Some(wait),
            inner: Some(inner),
        }
    }
}

pub struct RecvHalf {
    wait: Option<AsyncWait>,
    inner: Option<SyncRecvHalf>,
}

impl RecvHalf {
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let wait = self.wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("recv half already moved");
        recv_async(inner.receiver_mut(), wait).await
    }

    pub async fn recv_many(
        &mut self,
        out: &mut Vec<Vec<u8>>,
        max_messages: usize,
    ) -> Result<usize> {
        let wait = self.wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("recv half already moved");
        recv_many_async(inner.receiver_mut(), wait, out, max_messages).await
    }

    pub fn drain_ready(&mut self, out: &mut Vec<Vec<u8>>, max_messages: usize) -> Result<usize> {
        let inner = self.inner.as_mut().expect("recv half already moved");
        drain_ready(inner.receiver_mut(), out, max_messages)
    }

    pub async fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        let wait = self.wait.as_mut().expect("recv waiter missing");
        let inner = self.inner.as_mut().expect("recv half already moved");
        recv_into_async(inner.receiver_mut(), wait, buf).await
    }

    pub async fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        map_timeout(::tokio::time::timeout(timeout, self.recv())).await
    }

    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.inner
            .as_mut()
            .expect("recv half already moved")
            .try_recv()
    }

    fn from_sync(inner: SyncRecvHalf) -> Self {
        let wait = AsyncWait::new(inner.recv_wait_target(), inner.wait_spin_count());
        Self {
            wait: Some(wait),
            inner: Some(inner),
        }
    }
}

async fn send_async(sender: &mut RingSender, wait: &mut AsyncWait, payload: &[u8]) -> Result<()> {
    let spin_count = send_spin_count(wait.spin_count());
    loop {
        match spin_send(sender, payload, spin_count)? {
            TrySendResult::Sent => return Ok(()),
            TrySendResult::Full => {
                let observed = wait.snapshot()?;
                match spin_send(sender, payload, spin_count)? {
                    TrySendResult::Sent => return Ok(()),
                    TrySendResult::Full => wait.wait_changed(observed).await?,
                }
            }
        }
    }
}

async fn recv_async(receiver: &mut RingReceiver, wait: &mut AsyncWait) -> Result<Vec<u8>> {
    loop {
        match spin_peek_or_closed(receiver, wait.spin_count())? {
            Some(message) => return commit_message_to_vec(receiver, message),
            None => {
                let observed = wait.snapshot()?;
                match spin_peek_or_closed(receiver, wait.spin_count())? {
                    Some(message) => return commit_message_to_vec(receiver, message),
                    None => wait.wait_changed(observed).await?,
                }
            }
        }
    }
}

async fn recv_into_async(
    receiver: &mut RingReceiver,
    wait: &mut AsyncWait,
    buf: &mut [u8],
) -> Result<usize> {
    loop {
        match spin_peek_or_closed(receiver, wait.spin_count())? {
            Some(message) => return commit_message_into(receiver, message, buf),
            None => {
                let observed = wait.snapshot()?;
                match spin_peek_or_closed(receiver, wait.spin_count())? {
                    Some(message) => return commit_message_into(receiver, message, buf),
                    None => wait.wait_changed(observed).await?,
                }
            }
        }
    }
}

async fn recv_many_async(
    receiver: &mut RingReceiver,
    wait: &mut AsyncWait,
    out: &mut Vec<Vec<u8>>,
    max_messages: usize,
) -> Result<usize> {
    if max_messages == 0 {
        return Ok(0);
    }

    loop {
        let drained = drain_ready_with_spin(receiver, out, max_messages, wait.spin_count())?;
        if drained != 0 {
            return Ok(drained);
        }

        let observed = wait.snapshot()?;
        let drained = drain_ready_with_spin(receiver, out, max_messages, wait.spin_count())?;
        if drained != 0 {
            return Ok(drained);
        }

        wait.wait_changed(observed).await?;
    }
}

fn try_peek_or_closed(receiver: &RingReceiver) -> Result<Option<PeekedMessage>> {
    receiver.try_peek_or_closed()
}

fn spin_send(sender: &mut RingSender, payload: &[u8], spin_count: u32) -> Result<TrySendResult> {
    for _ in 0..spin_count {
        match sender.try_send_once(payload)? {
            TrySendResult::Sent => return Ok(TrySendResult::Sent),
            TrySendResult::Full => spin_loop(),
        }
    }

    sender.try_send_once(payload)
}

fn send_spin_count(spin_count: u32) -> u32 {
    spin_count.min(32)
}

fn spin_peek_or_closed(receiver: &RingReceiver, spin_count: u32) -> Result<Option<PeekedMessage>> {
    for _ in 0..spin_count {
        match try_peek_or_closed(receiver)? {
            Some(message) => return Ok(Some(message)),
            None => spin_loop(),
        }
    }

    try_peek_or_closed(receiver)
}

fn drain_ready_with_spin(
    receiver: &mut RingReceiver,
    out: &mut Vec<Vec<u8>>,
    max_messages: usize,
    spin_count: u32,
) -> Result<usize> {
    let start_len = out.len();
    while out.len() - start_len < max_messages {
        match spin_peek_or_closed(receiver, spin_count) {
            Ok(Some(message)) => out.push(commit_message_to_vec(receiver, message)?),
            Ok(None) => break,
            Err(Error::ChannelClosed) if out.len() != start_len => break,
            Err(err) => return Err(err),
        }
    }
    Ok(out.len() - start_len)
}

fn drain_ready(
    receiver: &mut RingReceiver,
    out: &mut Vec<Vec<u8>>,
    max_messages: usize,
) -> Result<usize> {
    let start_len = out.len();
    while out.len() - start_len < max_messages {
        match receiver.try_peek_message()? {
            TryPeekResult::Ready(message) => out.push(commit_message_to_vec(receiver, message)?),
            TryPeekResult::Empty => break,
        }
    }
    Ok(out.len() - start_len)
}

fn commit_message_to_vec(receiver: &mut RingReceiver, message: PeekedMessage) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; message.payload_len()];
    receiver.copy_current_message(message, &mut buf)?;
    receiver.commit_current_message(message)?;
    Ok(buf)
}

fn commit_message_into(
    receiver: &mut RingReceiver,
    message: PeekedMessage,
    buf: &mut [u8],
) -> Result<usize> {
    let payload_len = message.payload_len();
    if payload_len > buf.len() {
        receiver.commit_current_message(message)?;
        return Err(Error::MessageTooLarge {
            size: payload_len,
            max: buf.len(),
        });
    }

    receiver.copy_current_message(message, &mut buf[..payload_len])?;
    receiver.commit_current_message(message)?;
    Ok(payload_len)
}

async fn wait_for_channel_ready(name: &str, config: &ChannelConfig) -> Result<()> {
    let path = crate::platform::shm_path(name);
    let offsets = crate::header::RingOffsets::new(config.ring_size);
    let deadline = std::time::Instant::now() + config.connect_timeout;

    loop {
        if std::fs::metadata(&path)
            .map(|metadata| metadata.len() >= offsets.total_size as u64)
            .unwrap_or(false)
        {
            return Ok(());
        }

        if std::time::Instant::now() >= deadline {
            return Err(Error::TimedOut);
        }

        ::tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn map_timeout<T>(
    future: impl std::future::Future<
        Output = std::result::Result<Result<T>, ::tokio::time::error::Elapsed>,
    >,
) -> Result<T> {
    match future.await {
        Ok(value) => value,
        Err(_) => Err(Error::TimedOut),
    }
}
