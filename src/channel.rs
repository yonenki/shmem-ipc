use std::fs::OpenOptions;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use memmap2::MmapMut;

use crate::error::{Error, Result};
use crate::header::{
    ChannelState, GLOBAL_HEADER_SIZE, GlobalHeader, MAGIC, RingHeader, RingOffsets, VERSION,
};
use crate::platform;
#[cfg(feature = "tokio")]
use crate::ring::WaitTarget;
use crate::ring::{DEFAULT_RING_DATA_SIZE, RingReceiver, RingSender};
use crate::shared::SharedChannel;
use crate::wait::SpinThenWait;

/// チャネルの役割
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Server,
    Client,
}

/// チャネルの設定
#[derive(Clone)]
pub struct ChannelConfig {
    /// リングバッファのデータ領域サイズ (64以上の2の累乗)。デフォルト: 16 MiB
    pub ring_size: usize,
    /// 待機戦略。
    pub wait_strategy: SpinThenWait,
    /// 接続 identity の公開待ちタイムアウト。デフォルト: 5秒
    pub connect_timeout: Duration,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        Self {
            ring_size: DEFAULT_RING_DATA_SIZE,
            wait_strategy: SpinThenWait::default(),
            connect_timeout: Duration::from_secs(5),
        }
    }
}

/// 双方向 SPSC チャネル。全 ring / split half / waiter が同じ owner を保持する。
pub struct Channel {
    shared: Arc<SharedChannel>,
    sender: RingSender,
    receiver: RingReceiver,
    wait: SpinThenWait,
}

impl Channel {
    /// Server としてチャネルを作成する。
    ///
    /// ヘッダと birth identity を初期化して Client の接続を待てる状態にする。
    pub fn create(name: &str) -> Result<Self> {
        Self::create_with_config(name, ChannelConfig::default())
    }

    pub fn create_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        validate_config(&config)?;
        let identity = platform::current_identity()?;
        let path = platform::shm_path(name);
        let offsets = RingOffsets::new(config.ring_size);
        let wait_key = platform::new_wait_key();
        let wait_set = platform::ChannelWaitSet::new(name, wait_key)?;

        // Never truncate a mapping still owned by an earlier channel. Listener
        // incarnations give restarted listeners a different data-file name.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.set_len(offsets.total_size as u64)?;
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let base = mmap.as_ptr() as *mut u8;
        let gh = unsafe { &mut *(base as *mut GlobalHeader) };
        gh.magic = MAGIC;
        gh.version = VERSION;
        gh.ring_data_size = config.ring_size as u32;
        gh.server_pid = identity.pid;
        gh.server_birth = identity.birth;
        gh.wait_key = wait_key;
        gh.client_pid.store(0, Ordering::Relaxed);
        gh.client_birth.store(0, Ordering::Relaxed);

        for ring_header_offset in [offsets.ring_a_header, offsets.ring_b_header] {
            let rh = unsafe { &*(base.add(ring_header_offset) as *const RingHeader) };
            rh.writer.write_cursor.store(0, Ordering::Relaxed);
            rh.writer.notify.store(0, Ordering::Relaxed);
            rh.writer.parked.store(0, Ordering::Relaxed);
            rh.reader.read_cursor.store(0, Ordering::Relaxed);
            rh.reader.notify.store(0, Ordering::Relaxed);
            rh.reader.parked.store(0, Ordering::Relaxed);
        }
        // Publish the complete server identity and layout together.
        gh.state
            .store(ChannelState::ServerReady as u32, Ordering::Release);
        let shared = Arc::new(SharedChannel::new(
            mmap,
            wait_set,
            name.to_string(),
            Role::Server,
            config.connect_timeout,
            None,
        ));
        Ok(Self::from_shared(shared, config))
    }

    /// Server が作成した共有メモリのヘッダと process identity を検証して接続する。
    pub fn open(name: &str) -> Result<Self> {
        Self::open_with_config(name, ChannelConfig::default())
    }

    pub fn open_with_config(name: &str, config: ChannelConfig) -> Result<Self> {
        validate_config(&config)?;
        let identity = platform::current_identity()?;
        let deadline = Instant::now() + config.connect_timeout;
        loop {
            if let Some(channel) = Self::try_open(name, &config, identity)? {
                return Ok(channel);
            }
            if Instant::now() >= deadline {
                return Err(Error::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// One non-waiting readiness/claim attempt, shared by sync and Tokio opens.
    pub(crate) fn try_open(
        name: &str,
        config: &ChannelConfig,
        identity: platform::ProcessIdentity,
    ) -> Result<Option<Self>> {
        let file = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(platform::shm_path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if file.metadata()?.len() < GLOBAL_HEADER_SIZE as u64 {
            return Ok(None);
        }
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let gh = unsafe { GlobalHeader::from_ptr(mmap.as_ptr()) };
        // Acquire the publication before reading any immutable layout field.
        match ChannelState::from_u32(gh.state.load(Ordering::Acquire)) {
            Some(ChannelState::ServerReady) => {}
            Some(ChannelState::Init) => return Ok(None),
            Some(ChannelState::PeerDisconnected) => return Err(Error::PeerDisconnected),
            None => return Err(Error::InvalidHeader),
            _ => return Err(Error::ChannelClosed),
        }
        gh.validate()?;
        let offsets = RingOffsets::new(config.ring_size);
        if gh.ring_data_size as usize != config.ring_size || mmap.len() != offsets.total_size {
            return Err(Error::InvalidHeader);
        }
        let peer = platform::ProcessMonitor::open(platform::ProcessIdentity {
            pid: gh.server_pid,
            birth: gh.server_birth,
        })?;
        let wait_set = platform::ChannelWaitSet::new(name, gh.wait_key)?;

        // Claim exclusively, then publish both client identity words. Connected
        // must never be observable with a zero/mixed client identity.
        gh.state
            .compare_exchange(
                ChannelState::ServerReady as u32,
                ChannelState::ClientClaimed as u32,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|state| {
                if state == ChannelState::PeerDisconnected as u32 {
                    Error::PeerDisconnected
                } else {
                    Error::ChannelClosed
                }
            })?;
        gh.client_pid.store(identity.pid, Ordering::Relaxed);
        gh.client_birth.store(identity.birth, Ordering::Relaxed);
        gh.state
            .compare_exchange(
                ChannelState::ClientClaimed as u32,
                ChannelState::Connected as u32,
                Ordering::Release,
                Ordering::Acquire,
            )
            .map_err(|state| {
                if state == ChannelState::PeerDisconnected as u32 {
                    Error::PeerDisconnected
                } else {
                    Error::ChannelClosed
                }
            })?;
        let shared = Arc::new(SharedChannel::new(
            mmap,
            wait_set,
            name.to_string(),
            Role::Client,
            config.connect_timeout,
            Some(peer),
        ));
        shared.wake_all();
        Ok(Some(Self::from_shared(shared, config.clone())))
    }

    fn from_shared(shared: Arc<SharedChannel>, config: ChannelConfig) -> Self {
        let offsets = RingOffsets::new(config.ring_size);
        let base = shared.base();
        let (
            send_header,
            send_data,
            recv_header,
            recv_data,
            send_writer,
            send_reader,
            recv_writer,
            recv_reader,
        ) = match shared.role {
            Role::Server => (
                offsets.ring_a_header,
                offsets.ring_a_data,
                offsets.ring_b_header,
                offsets.ring_b_data,
                &shared.wait_set.ring_a_writer,
                &shared.wait_set.ring_a_reader,
                &shared.wait_set.ring_b_writer,
                &shared.wait_set.ring_b_reader,
            ),
            Role::Client => (
                offsets.ring_b_header,
                offsets.ring_b_data,
                offsets.ring_a_header,
                offsets.ring_a_data,
                &shared.wait_set.ring_b_writer,
                &shared.wait_set.ring_b_reader,
                &shared.wait_set.ring_a_writer,
                &shared.wait_set.ring_a_reader,
            ),
        };
        let sender = unsafe {
            RingSender::new(
                base,
                base.add(send_header) as *const RingHeader,
                Arc::clone(&shared),
                send_writer.clone(),
                send_reader.clone(),
                send_data,
                config.ring_size,
            )
        };
        let receiver = unsafe {
            RingReceiver::new(
                base,
                base.add(recv_header) as *const RingHeader,
                Arc::clone(&shared),
                recv_writer.clone(),
                recv_reader.clone(),
                recv_data,
                config.ring_size,
            )
        };
        Self {
            shared,
            sender,
            receiver,
            wait: config.wait_strategy,
        }
    }

    /// メッセージを送信する (ブロッキング、タイムアウトなし)。
    pub fn send(&mut self, payload: &[u8]) -> Result<()> {
        self.sender.send(payload, &self.wait, None)
    }

    /// メッセージを送信する (タイムアウト付き)。
    pub fn send_timeout(&mut self, payload: &[u8], timeout: Duration) -> Result<()> {
        self.sender.send(payload, &self.wait, Some(timeout))
    }

    /// メッセージを受信する (ブロッキング、タイムアウトなし)。
    pub fn recv(&mut self) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, None)
    }

    /// メッセージを受信する (タイムアウト付き)。
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        self.receiver.recv(&self.wait, Some(timeout))
    }

    /// 呼び出し元のバッファに受信する。
    pub fn recv_into(&mut self, buf: &mut [u8]) -> Result<usize> {
        self.receiver.recv_into(buf, &self.wait, None)
    }

    /// ノンブロッキング受信。空の終端チャネルでは保持した切断原因を返す。
    pub fn try_recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.try_recv()
    }

    /// Close both directions and wake every waiter without releasing its mapping.
    pub fn close(&self) -> Result<()> {
        self.shared.close()
    }

    /// チャネルの役割を取得する。
    pub fn role(&self) -> Role {
        self.shared.role
    }

    /// 最後に観測・公開したチャネル状態のスナップショット。
    pub fn state(&self) -> ChannelState {
        self.shared.state()
    }

    /// Explicit cleanup of a retired channel, only after its participants stop.
    pub fn cleanup(name: &str) -> Result<()> {
        match std::fs::remove_file(platform::shm_path(name)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// 同じ liveness / close authority を保持した ShmemConnection に変換する。
    pub fn into_connection(self) -> crate::connection::ShmemConnection {
        crate::connection::ShmemConnection::new(self.shared, self.sender, self.receiver, self.wait)
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn sender_mut(&mut self) -> &mut RingSender {
        &mut self.sender
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn receiver_mut(&mut self) -> &mut RingReceiver {
        &mut self.receiver
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn send_wait_target(&self) -> WaitTarget {
        self.sender.wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn recv_wait_target(&self) -> WaitTarget {
        self.receiver.wait_target()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn wait_spin_count(&self) -> u32 {
        self.wait.spin_count
    }
}

pub(crate) fn validate_config(config: &ChannelConfig) -> Result<()> {
    if config.ring_size < 64
        || !config.ring_size.is_power_of_two()
        || config.ring_size > u32::MAX as usize
    {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "ring_size must be a power of two between 64 and u32::MAX",
        )));
    }
    Ok(())
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn first_server_operation_rejects_a_reused_client_pid() {
        let name = format!(
            "birth_mismatch_{}_{}",
            std::process::id(),
            platform::new_wait_key()
        );
        let config = ChannelConfig {
            ring_size: 256,
            ..ChannelConfig::default()
        };
        let mut server = Channel::create_with_config(&name, config.clone()).unwrap();
        let mut client = Channel::open_with_config(&name, config).unwrap();
        let header = server.shared.header();
        let birth = header.client_birth.load(Ordering::Relaxed);
        header.client_birth.store(birth ^ 1, Ordering::Relaxed);
        assert!(matches!(
            server.send(b"not delivered"),
            Err(Error::PeerDisconnected)
        ));
        assert!(matches!(client.recv(), Err(Error::PeerDisconnected)));
        assert!(matches!(server.close(), Err(Error::PeerDisconnected)));
        assert!(matches!(
            client.send(b"still dead"),
            Err(Error::PeerDisconnected)
        ));
    }

    #[test]
    fn create_never_reinitializes_an_owned_mapping() {
        let name = format!(
            "owned_mapping_{}_{}",
            std::process::id(),
            platform::new_wait_key()
        );
        let config = ChannelConfig {
            ring_size: 256,
            ..ChannelConfig::default()
        };
        let mut server = Channel::create_with_config(&name, config.clone()).unwrap();
        let mut client = Channel::open_with_config(&name, config.clone()).unwrap();
        client.send(b"preserved").unwrap();
        assert!(
            matches!(Channel::create_with_config(&name, config), Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::AlreadyExists)
        );
        assert_eq!(server.recv().unwrap(), b"preserved");
        server.send(b"still alive").unwrap();
        assert_eq!(client.recv().unwrap(), b"still alive");
    }

    #[test]
    fn claimed_client_cannot_publish_messages_before_its_identity_is_ready() {
        let name = format!(
            "claimed_client_{}_{}",
            std::process::id(),
            platform::new_wait_key()
        );
        let config = ChannelConfig {
            ring_size: 256,
            ..ChannelConfig::default()
        };
        let mut server = Channel::create_with_config(&name, config.clone()).unwrap();
        let mut client = Channel::open_with_config(&name, config).unwrap();
        let shared = Arc::clone(&server.shared);
        let header = shared.header();
        let birth = header.client_birth.load(Ordering::Relaxed);
        header
            .state
            .store(ChannelState::ClientClaimed as u32, Ordering::Release);
        header.client_birth.store(0, Ordering::Relaxed);
        assert!(matches!(
            server.send_timeout(b"premature", Duration::from_millis(20)),
            Err(Error::TimedOut)
        ));
        header.client_birth.store(birth, Ordering::Relaxed);
        header
            .state
            .store(ChannelState::Connected as u32, Ordering::Release);
        server.send(b"ready").unwrap();
        assert_eq!(client.recv().unwrap(), b"ready");
        assert!(client.try_recv().unwrap().is_none());
    }

    #[cfg(feature = "tokio")]
    #[::tokio::test]
    async fn opening_an_unpublished_mapping_does_not_block_the_executor() {
        let name = format!(
            "unpublished_{}_{}",
            std::process::id(),
            platform::new_wait_key()
        );
        let config = ChannelConfig {
            ring_size: 256,
            ..ChannelConfig::default()
        };
        let mut server = Channel::create_with_config(&name, config.clone()).unwrap();
        let shared = Arc::clone(&server.shared);
        shared
            .header()
            .state
            .store(ChannelState::Init as u32, Ordering::Release);
        let publish = async {
            ::tokio::time::sleep(Duration::from_millis(20)).await;
            shared
                .header()
                .state
                .store(ChannelState::ServerReady as u32, Ordering::Release);
        };
        let (client, ()) = ::tokio::time::timeout(Duration::from_secs(1), async {
            ::tokio::join!(
                crate::tokio::Channel::open_with_config(&name, config),
                publish
            )
        })
        .await
        .expect("publication task must run while open is waiting");
        let mut client = client.unwrap();
        server.send(b"published").unwrap();
        assert_eq!(client.recv().await.unwrap(), b"published");
    }
}
