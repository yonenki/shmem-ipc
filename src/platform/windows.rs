use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

#[cfg(feature = "tokio")]
use std::sync::Mutex;
#[cfg(feature = "tokio")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "tokio")]
use std::task::{Context, Poll, Waker};

#[cfg(feature = "tokio")]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateEventW,
    CreateWaitableTimerExW, GetCurrentProcessId, GetExitCodeProcess, OpenProcess, SetEvent,
    SetWaitableTimerEx, TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};
#[cfg(feature = "tokio")]
use windows_sys::Win32::System::Threading::{RegisterWaitForSingleObject, UnregisterWaitEx};

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

#[derive(Clone, Copy)]
enum SignalKind {
    Sync,
    Async,
}

impl SignalKind {
    fn suffix(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Async => "async",
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
    async_event: HANDLE,
    timeout_timer: HANDLE,
    #[cfg(feature = "tokio")]
    async_state: Box<AsyncWaitState>,
}

unsafe impl Send for WaitObjects {}
unsafe impl Sync for WaitObjects {}

#[cfg(feature = "tokio")]
struct AsyncWaitState {
    wait_object: Mutex<HANDLE>,
    signaled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

#[cfg(feature = "tokio")]
impl AsyncWaitState {
    fn new() -> Self {
        Self {
            wait_object: Mutex::new(std::ptr::null_mut()),
            signaled: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    fn prepare_wait(&self, event: HANDLE) -> io::Result<()> {
        self.ensure_registered(event)
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.signaled.swap(false, Ordering::AcqRel) {
            return Poll::Ready(Ok(()));
        }

        let mut waker = lock_unpoison(&self.waker);
        if !waker
            .as_ref()
            .is_some_and(|stored| stored.will_wake(cx.waker()))
        {
            *waker = Some(cx.waker().clone());
        }

        if self.signaled.swap(false, Ordering::AcqRel) {
            *waker = None;
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn clear_waiter(&self) {
        self.signaled.store(false, Ordering::Release);
        let mut waker = lock_unpoison(&self.waker);
        *waker = None;
    }

    fn ensure_registered(&self, event: HANDLE) -> io::Result<()> {
        let mut wait_object = lock_unpoison(&self.wait_object);
        if !wait_object.is_null() {
            return Ok(());
        }

        let mut new_wait = std::ptr::null_mut();
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut new_wait,
                event,
                Some(wait_callback),
                self as *const Self as *const _,
                INFINITE,
                0,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        *wait_object = new_wait;
        Ok(())
    }
}

impl Drop for WaitObjects {
    fn drop(&mut self) {
        #[cfg(feature = "tokio")]
        {
            let wait_object = self
                .async_state
                .wait_object
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !wait_object.is_null() {
                let ok = unsafe { UnregisterWaitEx(*wait_object, INVALID_HANDLE_VALUE) };
                if ok == 0 {
                    let err = io::Error::last_os_error();
                    panic!("shmem-ipc: UnregisterWaitEx failed: {err}");
                }
            }
        }

        if !self.event.is_null() {
            unsafe {
                CloseHandle(self.event);
            }
        }
        if !self.async_event.is_null() {
            unsafe {
                CloseHandle(self.async_event);
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
        let sync_name = event_name(channel_name, wait_key, slot, SignalKind::Sync);
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, sync_name.as_ptr()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }

        let async_name = event_name(channel_name, wait_key, slot, SignalKind::Async);
        let async_event = unsafe { CreateEventW(std::ptr::null(), 0, 0, async_name.as_ptr()) };
        if async_event.is_null() {
            unsafe {
                CloseHandle(event);
            }
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
                CloseHandle(async_event);
            }
            return Err(io::Error::last_os_error());
        }

        #[cfg(feature = "tokio")]
        let async_state = Box::new(AsyncWaitState::new());

        Ok(Self {
            inner: Arc::new(WaitObjects {
                event,
                async_event,
                timeout_timer,
                #[cfg(feature = "tokio")]
                async_state,
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

    #[cfg(feature = "tokio")]
    {
        let ok = unsafe { SetEvent(handle.inner.async_event) };
        if ok == 0 {
            let err = io::Error::last_os_error();
            panic!("shmem-ipc: SetEvent failed: {err}");
        }
    }
}

#[cfg(feature = "tokio")]
pub fn prepare_wait_handle(handle: &WaitHandle) -> io::Result<()> {
    handle
        .inner
        .async_state
        .prepare_wait(handle.inner.async_event)
}

#[cfg(feature = "tokio")]
pub fn poll_wait_handle(handle: &WaitHandle, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    handle.inner.async_state.poll(cx)
}

#[cfg(feature = "tokio")]
pub fn clear_wait_handle(handle: &WaitHandle) {
    handle.inner.async_state.clear_waiter();
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

fn event_name(channel_name: &str, wait_key: u64, slot: WaitSlot, kind: SignalKind) -> Vec<u16> {
    let name_hash = stable_name_hash(channel_name);
    format!(
        "Local\\shmem_ipc_{name_hash:016x}_{wait_key:016x}_{}_{}",
        slot.suffix(),
        kind.suffix()
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

#[cfg(feature = "tokio")]
fn lock_unpoison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(feature = "tokio")]
unsafe extern "system" fn wait_callback(context: *mut core::ffi::c_void, _timed_out: bool) {
    let state = unsafe { &*(context as *const AsyncWaitState) };
    state.signaled.store(true, Ordering::Release);

    let waker = {
        let waker = lock_unpoison(&state.waker);
        waker.as_ref().cloned()
    };

    if let Some(waker) = waker {
        waker.wake();
    }
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
