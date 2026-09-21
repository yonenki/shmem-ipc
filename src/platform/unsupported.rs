use std::sync::atomic::AtomicU32;
use std::time::Duration;

#[derive(Clone)]
pub enum WaitHandle {}

pub struct ChannelWaitSet {
    pub ring_a_writer: WaitHandle,
    pub ring_a_reader: WaitHandle,
    pub ring_b_writer: WaitHandle,
    pub ring_b_reader: WaitHandle,
}

impl ChannelWaitSet {
    pub fn new(_name: &str, _key: u64) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            crate::Error::UnsupportedPlatform,
        ))
    }
}

pub fn wait_on_handle(
    handle: &WaitHandle,
    _word: &AtomicU32,
    _expected: u32,
    _timeout: Option<Duration>,
) {
    match *handle {}
}

pub fn wake_handle(handle: &WaitHandle, _word: &AtomicU32) {
    match *handle {}
}

pub enum ProcessMonitor {}

impl ProcessMonitor {
    pub fn open(_identity: super::ProcessIdentity) -> crate::Result<Self> {
        Err(crate::Error::UnsupportedPlatform)
    }

    pub fn is_alive(&self) -> crate::Result<bool> {
        match *self {}
    }
}

pub fn current_pid() -> u64 {
    u64::from(std::process::id())
}

pub fn current_identity() -> crate::Result<super::ProcessIdentity> {
    Err(crate::Error::UnsupportedPlatform)
}
