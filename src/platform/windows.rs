use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
    CreateWaitableTimerExW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, SetEvent,
    SetWaitableTimerEx, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
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
    inner: Arc<WaitObjects>,
}

#[derive(Clone)]
pub struct ChannelWaitSet {
    pub ring_a_writer: WaitHandle,
    pub ring_a_reader: WaitHandle,
    pub ring_b_writer: WaitHandle,
    pub ring_b_reader: WaitHandle,
}

struct WaitObjects {
    event: HANDLE,
    timeout_timer: HANDLE,
}

unsafe impl Send for WaitObjects {}
unsafe impl Sync for WaitObjects {}

impl Drop for WaitObjects {
    fn drop(&mut self) {
        if !self.event.is_null() {
            unsafe {
                CloseHandle(self.event);
            }
        }
        if !self.timeout_timer.is_null() {
            unsafe {
                CloseHandle(self.timeout_timer);
            }
        }
    }
}

impl WaitHandle {
    fn open(channel_name: &str, wait_key: u64, slot: WaitSlot) -> io::Result<Self> {
        let name = event_name(channel_name, wait_key, slot);
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }

        let timeout_timer = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if timeout_timer.is_null() {
            unsafe {
                CloseHandle(event);
            }
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            inner: Arc::new(WaitObjects {
                event,
                timeout_timer,
            }),
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
    match timeout {
        None => wait_for_event(handle.inner.event, INFINITE),
        Some(timeout) if timeout.is_zero() => wait_for_event(handle.inner.event, 0),
        Some(timeout) => wait_for_event_or_timeout(handle, timeout),
    }
}

pub fn wake_handle(handle: &WaitHandle, _word: &AtomicU32) {
    let ok = unsafe { SetEvent(handle.inner.event) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        panic!("shmem-ipc: SetEvent failed: {err}");
    }
}

fn wait_for_event(event: HANDLE, timeout_ms: u32) {
    let result = unsafe { WaitForSingleObject(event, timeout_ms) };
    match result {
        WAIT_OBJECT_0 | WAIT_TIMEOUT => {}
        WAIT_FAILED => {
            let err = io::Error::last_os_error();
            panic!("shmem-ipc: WaitForSingleObject failed: {err}");
        }
        unexpected => {
            panic!("shmem-ipc: WaitForSingleObject returned unexpected status {unexpected}");
        }
    }
}

fn wait_for_event_or_timeout(handle: &WaitHandle, timeout: Duration) {
    arm_timeout_timer(handle.inner.timeout_timer, timeout);

    let handles = [handle.inner.event, handle.inner.timeout_timer];
    let result =
        unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, INFINITE) };
    match result {
        WAIT_OBJECT_0 => disarm_timeout_timer(handle.inner.timeout_timer),
        value if value == WAIT_OBJECT_0 + 1 => {}
        WAIT_FAILED => {
            let err = io::Error::last_os_error();
            panic!("shmem-ipc: WaitForMultipleObjects failed: {err}");
        }
        unexpected => {
            panic!("shmem-ipc: WaitForMultipleObjects returned unexpected status {unexpected}");
        }
    }
}

fn arm_timeout_timer(timer: HANDLE, timeout: Duration) {
    let due_time = timeout_to_due_time(timeout);
    let ok = unsafe {
        SetWaitableTimerEx(
            timer,
            &due_time,
            0,
            None,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if ok == 0 {
        let err = io::Error::last_os_error();
        panic!("shmem-ipc: SetWaitableTimerEx failed: {err}");
    }
}

fn disarm_timeout_timer(timer: HANDLE) {
    let ok = unsafe { CancelWaitableTimer(timer) };
    if ok == 0 {
        let err = io::Error::last_os_error();
        panic!("shmem-ipc: CancelWaitableTimer failed: {err}");
    }

    // Synchronization timers reset only when their wait is consumed.
    // Drain any already-signaled timer so the next wait cannot observe a stale timeout.
    wait_for_event(timer, 0);
}

fn timeout_to_due_time(timeout: Duration) -> i64 {
    let ticks_100ns = timeout.as_nanos().div_ceil(100).min(i64::MAX as u128) as i64;
    -ticks_100ns
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
