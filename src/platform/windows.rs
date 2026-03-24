use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, SetEvent,
    WaitForSingleObject,
};

const INFINITE: u32 = 0xFFFF_FFFF;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;

#[derive(Clone, Copy)]
enum WaitSlot {
    RingAWriter,
    RingAReader,
    RingBWriter,
    RingBReader,
}

impl WaitSlot {
    fn suffix(self) -> &'static str {
        match self {
            Self::RingAWriter => "a_writer",
            Self::RingAReader => "a_reader",
            Self::RingBWriter => "b_writer",
            Self::RingBReader => "b_reader",
        }
    }
}

#[derive(Clone)]
pub struct WaitHandle {
    inner: Arc<NamedEvent>,
}

#[derive(Clone)]
pub struct ChannelWaitSet {
    pub ring_a_writer: WaitHandle,
    pub ring_a_reader: WaitHandle,
    pub ring_b_writer: WaitHandle,
    pub ring_b_reader: WaitHandle,
}

struct NamedEvent {
    handle: HANDLE,
}

unsafe impl Send for NamedEvent {}
unsafe impl Sync for NamedEvent {}

impl Drop for NamedEvent {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

impl WaitHandle {
    fn open(channel_name: &str, wait_key: u64, slot: WaitSlot) -> io::Result<Self> {
        let name = event_name(channel_name, wait_key, slot);
        let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            inner: Arc::new(NamedEvent { handle }),
        })
    }
}

impl ChannelWaitSet {
    pub fn new(channel_name: &str, wait_key: u64) -> io::Result<Self> {
        Ok(Self {
            ring_a_writer: WaitHandle::open(channel_name, wait_key, WaitSlot::RingAWriter)?,
            ring_a_reader: WaitHandle::open(channel_name, wait_key, WaitSlot::RingAReader)?,
            ring_b_writer: WaitHandle::open(channel_name, wait_key, WaitSlot::RingBWriter)?,
            ring_b_reader: WaitHandle::open(channel_name, wait_key, WaitSlot::RingBReader)?,
        })
    }
}

pub fn wait_on_handle(
    handle: &WaitHandle,
    _word: &AtomicU32,
    _expected: u32,
    timeout: Option<Duration>,
) {
    let result = unsafe { WaitForSingleObject(handle.inner.handle, timeout_to_millis(timeout)) };
    if result == WAIT_FAILED {
        let err = io::Error::last_os_error();
        panic!("shmem-ipc: WaitForSingleObject failed: {err}");
    }
}

pub fn wake_handle(handle: &WaitHandle, _word: &AtomicU32) {
    let ok = unsafe { SetEvent(handle.inner.handle) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        panic!("shmem-ipc: SetEvent failed: {err}");
    }
}

fn timeout_to_millis(timeout: Option<Duration>) -> u32 {
    match timeout {
        None => INFINITE,
        Some(timeout) => {
            let millis = timeout.as_millis();
            if millis == 0 {
                if timeout.is_zero() { 0 } else { 1 }
            } else {
                millis.min((u32::MAX - 1) as u128) as u32
            }
        }
    }
}

fn event_name(channel_name: &str, wait_key: u64, slot: WaitSlot) -> Vec<u16> {
    let name_hash = stable_name_hash(channel_name);
    format!(
        "Local\\shmem_ipc_{name_hash:016x}_{wait_key:016x}_{}",
        slot.suffix()
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect()
}

fn stable_name_hash(channel_name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in channel_name.as_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn current_pid() -> u64 {
    unsafe { GetCurrentProcessId() as u64 }
}

#[allow(dead_code)]
pub fn is_process_alive(pid: u64) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
        if handle.is_null() {
            return false;
        }

        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}
