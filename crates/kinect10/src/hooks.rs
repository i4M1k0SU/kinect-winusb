//! Hooks installed into the official Kinect10.dll's import table.
//!
//! Only accesses to the Kinect camera device interface
//! (`{00873FDF-61A8-11D1-EF5E-00C04F2D728B}`) are intercepted; everything else is
//! passed straight to kernel32. Each emulated file gets a real (event) handle as
//! its handle value so that handle values stay unique and stray kernel calls fail
//! gracefully instead of touching an unrelated object.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

use kinect_usb::camera::{self, CameraFile, NtStatus, ReadRequest, UserBuffer};
use kinect_usb::{lock_unless_exiting, process_exiting};
use log::{debug, error, info, trace};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{CreateFileA, CreateFileW, FILE_FLAG_OVERLAPPED, ReadFile};
use windows_sys::Win32::System::IO::{
    CreateIoCompletionPort, DeviceIoControl, GetOverlappedResult, OVERLAPPED, PostQueuedCompletionStatus,
};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, ResetEvent, SetEvent, WaitForSingleObject};
use windows_sys::core::{BOOL, w};

/// Lower-case interface class GUID string of the camera device interface.
const CAMERA_GUID_STR: &str = "{00873fdf-61a8-11d1-ef5e-00c04f2d728b}";

struct EmuFile {
    file: CameraFile,
    /// The handle value handed out (a private event object).
    handle: HANDLE,
    overlapped: bool,
    /// Completion port and key the handle is associated with.
    iocp: Mutex<Option<(HANDLE, usize)>>,
}

unsafe impl Send for EmuFile {}
unsafe impl Sync for EmuFile {}

type Table = HashMap<usize, Arc<EmuFile>>;

/// Emulated files by handle value.
static TABLE: LazyLock<RwLock<Table>> = LazyLock::new(Default::default);

/// Number of emulated handles currently open (fast path check).
static OPEN_COUNT: AtomicUsize = AtomicUsize::new(0);

// When the application exits without `NuiShutdown`, the official runtime stops
// its streams and closes its handles from `DLL_PROCESS_DETACH`, after every
// other thread was terminated, possibly one holding the table lock. So during
// process exit the lock is only tried: a handle that cannot be looked up goes
// to kernel32, which fails the call on the event object behind it.
fn read_table() -> Option<RwLockReadGuard<'static, Table>> {
    if process_exiting() { TABLE.try_read().ok() } else { TABLE.read().ok() }
}

fn write_table() -> Option<RwLockWriteGuard<'static, Table>> {
    if process_exiting() { TABLE.try_write().ok() } else { TABLE.write().ok() }
}

fn lookup(h: HANDLE) -> Option<Arc<EmuFile>> {
    if OPEN_COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    read_table()?.get(&h.addr()).cloned()
}

pub unsafe fn install(module: HMODULE) {
    let m = module.cast_const().cast();
    let hooks: [(&str, usize); 7] = [
        ("CreateFileA", hook_create_file_a as *const () as usize),
        ("CreateFileW", hook_create_file_w as *const () as usize),
        ("DeviceIoControl", hook_device_io_control as *const () as usize),
        ("ReadFile", hook_read_file as *const () as usize),
        ("CloseHandle", hook_close_handle as *const () as usize),
        ("CreateIoCompletionPort", hook_create_io_completion_port as *const () as usize),
        ("GetOverlappedResult", hook_get_overlapped_result as *const () as usize),
    ];
    for (name, f) in hooks {
        match unsafe { crate::iat::patch(m, "KERNEL32.dll", name, f) } {
            Some(_) => debug!("hooked {name}"),
            None => error!("could not hook {name} in the official runtime"),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers

fn nt_success(s: NtStatus) -> bool {
    s >= 0
}

fn nt_error(s: NtStatus) -> bool {
    s.cast_unsigned() >> 30 == 3
}

fn status_to_win32(s: NtStatus) -> u32 {
    unsafe { RtlNtStatusToDosError(s) }
}

type NtSetIoCompletionFn = unsafe extern "system" fn(HANDLE, usize, *mut c_void, NtStatus, usize) -> NtStatus;

static NT_SET_IO_COMPLETION: LazyLock<Option<NtSetIoCompletionFn>> = LazyLock::new(|| unsafe {
    let ntdll = GetModuleHandleW(w!("ntdll.dll"));
    GetProcAddress(ntdll, c"NtSetIoCompletion".as_ptr().cast())
        .map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, NtSetIoCompletionFn>(f))
});

/// Complete an operation described by `ov` the way the I/O manager does:
/// fill in the status block, signal the event and queue a completion packet.
unsafe fn complete_overlapped(ef: &EmuFile, ov: *mut OVERLAPPED, status: NtStatus, info: usize, queue: bool) {
    unsafe {
        // The low bit of hEvent set means "no completion packet".
        let hevent = (*ov).hEvent;
        (*ov).InternalHigh = info;
        AtomicUsize::from_ptr(&raw mut (*ov).Internal).store(status.cast_unsigned() as usize, Ordering::Release);
        let event = hevent.map_addr(|a| a & !1);
        SetEvent(if event.is_null() { ef.handle } else { event });
        if queue && hevent.addr() & 1 == 0 {
            // During process exit the packet may be lost (see `read_table`), but
            // then nothing reads the port anymore.
            let iocp = lock_unless_exiting(&ef.iocp).and_then(|g| *g);
            if let Some((port, key)) = iocp {
                match *NT_SET_IO_COMPLETION {
                    Some(f) => {
                        f(port, key, ov.cast(), status, info);
                    }
                    None => {
                        PostQueuedCompletionStatus(port, info as u32, key, ov);
                    }
                }
            }
        }
    }
}

/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 string.
unsafe fn from_wide_ptr(p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }))
}

/// # Safety
/// `p` must be null or point to a NUL-terminated byte string.
unsafe fn from_ansi_ptr(p: *const u8) -> Option<String> {
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p.cast()) }.to_string_lossy().into_owned())
}

fn camera_path(path: &str) -> Option<(String, String)> {
    let lower = path.to_ascii_lowercase();
    let i = lower.find(CAMERA_GUID_STR)? + CAMERA_GUID_STR.len();
    Some((path[..i].to_string(), path[i..].to_string()))
}

unsafe fn open_emulated(base: &str, suffix: &str, flags: u32) -> HANDLE {
    match camera::open(base, suffix) {
        Ok(file) => unsafe {
            let ev = CreateEventW(null(), 1, 0, null());
            if ev.is_null() {
                return INVALID_HANDLE_VALUE;
            }
            let ef = Arc::new(EmuFile {
                file,
                handle: ev,
                overlapped: flags & FILE_FLAG_OVERLAPPED != 0,
                iocp: Mutex::new(None),
            });
            TABLE.write().unwrap().insert(ev.addr(), ef);
            OPEN_COUNT.fetch_add(1, Ordering::AcqRel);
            info!("emulated open {base}{suffix} -> handle {:p}", ev);
            SetLastError(0);
            ev
        },
        Err(st) => {
            let e = status_to_win32(st);
            error!("emulated open of {base}{suffix} failed: status 0x{:08X} (win32 {e})", st.cast_unsigned());
            unsafe { SetLastError(e) };
            INVALID_HANDLE_VALUE
        }
    }
}

// ---------------------------------------------------------------------------
// hooks

unsafe extern "system" fn hook_create_file_a(
    name: *const u8,
    access: u32,
    share: u32,
    sa: *const SECURITY_ATTRIBUTES,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    if let Some(p) = unsafe { from_ansi_ptr(name) }
        && let Some((base, suffix)) = camera_path(&p)
    {
        return unsafe { open_emulated(&base, &suffix, flags) };
    }
    unsafe { CreateFileA(name, access, share, sa, disposition, flags, template) }
}

unsafe extern "system" fn hook_create_file_w(
    name: *const u16,
    access: u32,
    share: u32,
    sa: *const SECURITY_ATTRIBUTES,
    disposition: u32,
    flags: u32,
    template: HANDLE,
) -> HANDLE {
    if let Some(p) = unsafe { from_wide_ptr(name) }
        && let Some((base, suffix)) = camera_path(&p)
    {
        return unsafe { open_emulated(&base, &suffix, flags) };
    }
    unsafe { CreateFileW(name, access, share, sa, disposition, flags, template) }
}

unsafe extern "system" fn hook_device_io_control(
    h: HANDLE,
    code: u32,
    inbuf: *const c_void,
    insize: u32,
    outbuf: *mut c_void,
    outsize: u32,
    returned: *mut u32,
    ov: *mut OVERLAPPED,
) -> BOOL {
    let Some(ef) = lookup(h) else {
        return unsafe { DeviceIoControl(h, code, inbuf, insize, outbuf, outsize, returned, ov) };
    };
    // METHOD_BUFFERED semantics: input is captured before any output is written
    // (callers pass the same buffer for both).
    let input: Vec<u8> = if inbuf.is_null() || insize == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(inbuf.cast::<u8>(), insize as usize).to_vec() }
    };
    let mut output = vec![0u8; if outbuf.is_null() { 0 } else { outsize as usize }];
    let (status, info) = ef.file.ioctl(code, &input, &mut output);
    let info = info.min(output.len());
    trace!("ioctl 0x{code:08X} in={insize} out={outsize} -> 0x{:08X} info={info}", status.cast_unsigned());
    if !nt_error(status) && info > 0 {
        unsafe { std::ptr::copy_nonoverlapping(output.as_ptr(), outbuf.cast::<u8>(), info) };
    }
    unsafe {
        if !returned.is_null() {
            *returned = if nt_error(status) { 0 } else { info as u32 };
        }
        if !ov.is_null() {
            complete_overlapped(&ef, ov, status, if nt_error(status) { 0 } else { info }, nt_success(status));
        }
        if nt_success(status) {
            SetLastError(0);
            TRUE
        } else {
            SetLastError(status_to_win32(status));
            FALSE
        }
    }
}

unsafe extern "system" fn hook_read_file(
    h: HANDLE,
    buf: *mut u8,
    len: u32,
    read: *mut u32,
    ov: *mut OVERLAPPED,
) -> BOOL {
    let Some(ef) = lookup(h) else {
        return unsafe { ReadFile(h, buf, len, read, ov) };
    };
    unsafe {
        if !read.is_null() {
            *read = 0;
        }
        if ov.is_null() || !ef.overlapped {
            // Synchronous read: wait for completion here.
            let done = Arc::new(OnceLock::<(NtStatus, usize)>::new());
            let d2 = done.clone();
            let req = ReadRequest::new(
                UserBuffer { ptr: buf, len: len as usize },
                Box::new(move |st, info| {
                    let _ = d2.set((st, info));
                }),
            );
            ef.file.read(req);
            let (st, info) = *done.wait();
            if !ov.is_null() {
                complete_overlapped(&ef, ov, st, info, false);
            }
            if !read.is_null() {
                *read = info as u32;
            }
            if nt_success(st) {
                SetLastError(0);
                return TRUE;
            }
            SetLastError(status_to_win32(st));
            return FALSE;
        }

        (*ov).InternalHigh = 0;
        AtomicUsize::from_ptr(&raw mut (*ov).Internal)
            .store(STATUS_PENDING.cast_unsigned() as usize, Ordering::Release);
        let event = (*ov).hEvent.map_addr(|a| a & !1);
        ResetEvent(if event.is_null() { ef.handle } else { event });
        trace!("ReadFile {:p} len {len} ov {:p} hEvent {:p}", h, ov, (*ov).hEvent);
        let ef2 = ef.clone();
        // The completion runs on another thread: carry the OVERLAPPED as its address.
        let ov_addr = ov.expose_provenance();
        let req = ReadRequest::new(
            UserBuffer { ptr: buf, len: len as usize },
            Box::new(move |st, info| {
                trace!("ReadFile ov {:#x} completed: 0x{:08X} info {info}", ov_addr, st.cast_unsigned());
                complete_overlapped(&ef2, std::ptr::with_exposed_provenance_mut(ov_addr), st, info, true);
            }),
        );
        ef.file.read(req);
        SetLastError(ERROR_IO_PENDING);
        FALSE
    }
}

unsafe extern "system" fn hook_close_handle(h: HANDLE) -> BOOL {
    // Kinect10.dll closes many unrelated handles (events, threads): only take
    // the write lock for handles that are actually ours.
    if lookup(h).is_some()
        && let Some(ef) = write_table().and_then(|mut t| t.remove(&h.addr()))
    {
        OPEN_COUNT.fetch_sub(1, Ordering::AcqRel);
        info!("emulated close handle {:p}", h);
        // Cancel this file's queued reads. The event object behind the handle
        // value is closed when the last reference (held by in-flight
        // completions) goes away, see `Drop for EmuFile`.
        ef.file.cleanup();
        return TRUE;
    }
    unsafe { CloseHandle(h) }
}

impl Drop for EmuFile {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

unsafe extern "system" fn hook_create_io_completion_port(
    file: HANDLE,
    existing: HANDLE,
    key: usize,
    threads: u32,
) -> HANDLE {
    if file != INVALID_HANDLE_VALUE
        && let Some(ef) = lookup(file)
    {
        let port = if existing.is_null() {
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, null_mut(), 0, threads) }
        } else {
            existing
        };
        if !port.is_null() {
            *ef.iocp.lock().unwrap() = Some((port, key));
            debug!("handle {:p} associated with IOCP {:p} key {key:#x}", file, port);
        }
        return port;
    }
    unsafe { CreateIoCompletionPort(file, existing, key, threads) }
}

unsafe extern "system" fn hook_get_overlapped_result(
    h: HANDLE,
    ov: *const OVERLAPPED,
    transferred: *mut u32,
    wait: BOOL,
) -> BOOL {
    let Some(ef) = lookup(h) else {
        return unsafe { GetOverlappedResult(h, ov, transferred, wait) };
    };
    unsafe {
        let internal = AtomicUsize::from_ptr((&raw const (*ov).Internal).cast_mut());
        loop {
            let st = (internal.load(Ordering::Acquire) as u32).cast_signed();
            if st != STATUS_PENDING {
                if !transferred.is_null() {
                    *transferred = (*ov).InternalHigh as u32;
                }
                if nt_success(st) {
                    return TRUE;
                }
                SetLastError(status_to_win32(st));
                return FALSE;
            }
            if wait == 0 {
                SetLastError(ERROR_IO_INCOMPLETE);
                return FALSE;
            }
            let event = (*ov).hEvent.map_addr(|a| a & !1);
            let (target, timeout) = if event.is_null() { (ef.handle, 10) } else { (event, INFINITE) };
            WaitForSingleObject(target, timeout);
        }
    }
}
