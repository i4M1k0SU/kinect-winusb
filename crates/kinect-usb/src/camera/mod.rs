//! User-mode emulation of `kinectcamera.sys` (see `docs/kinectcamera-sys.md`).
//!
//! The public surface mirrors what the kernel driver exposes to `Kinect10.dll`:
//! a file per `CreateFile` (control handle or isochronous pipe handle), buffered
//! IOCTLs and asynchronous stream reads.

mod device;
mod protocol;
mod stream;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};

pub use device::Camera;
pub use stream::now_us;

pub type NtStatus = i32;

/// NTSTATUS values used by the driver.
pub mod status {
    use super::NtStatus;
    pub const SUCCESS: NtStatus = 0;
    pub const UNSUCCESSFUL: NtStatus = 0xC000_0001_u32.cast_signed();
    pub const INVALID_PARAMETER: NtStatus = 0xC000_000D_u32.cast_signed();
    pub const NO_SUCH_DEVICE: NtStatus = 0xC000_000E_u32.cast_signed();
    pub const INVALID_DEVICE_REQUEST: NtStatus = 0xC000_0010_u32.cast_signed();
    pub const BUFFER_TOO_SMALL: NtStatus = 0xC000_0023_u32.cast_signed();
    pub const OBJECT_NAME_NOT_FOUND: NtStatus = 0xC000_0034_u32.cast_signed();
    pub const DEVICE_BUSY: NtStatus = 0x8000_0011_u32.cast_signed();
    pub const CANCELLED: NtStatus = 0xC000_0120_u32.cast_signed();
    pub const INVALID_DEVICE_STATE: NtStatus = 0xC000_0184_u32.cast_signed();
    pub const IO_DEVICE_ERROR: NtStatus = 0xC000_0185_u32.cast_signed();
    pub const REQUEST_ABORTED: NtStatus = 0xC000_0240_u32.cast_signed();
    pub const ACCESS_DENIED: NtStatus = 0xC000_0022_u32.cast_signed();
    pub const SHARING_VIOLATION: NtStatus = 0xC000_0043_u32.cast_signed();
    /// What KMDF returns when forwarding to a purged manual queue.
    pub const WDF_BUSY: NtStatus = 0xC042_5001_u32.cast_signed();

    /// Translate a WinUSB / CreateFile error into an NTSTATUS.
    pub fn from_error(e: &windows::core::Error) -> NtStatus {
        let code = windows::Win32::Foundation::WIN32_ERROR::from_error(e).map_or(0, |w| w.0);
        match code {
            2 | 3 => OBJECT_NAME_NOT_FOUND,
            5 => ACCESS_DENIED,
            32 => SHARING_VIOLATION,
            87 => INVALID_PARAMETER,
            995 => CANCELLED,
            1167 | 22 | 433 => NO_SUCH_DEVICE,
            _ => IO_DEVICE_ERROR,
        }
    }
}

/// Lock `m` for a stop or cleanup. The official runtime still stops its streams
/// from `DLL_PROCESS_DETACH` when the application exits without `NuiShutdown`;
/// by then `ExitProcess` has terminated every other thread, possibly one that
/// held `m`. So during process exit this gives `None` instead of waiting forever.
fn lock_unless_exiting<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    if crate::process_exiting() { m.try_lock().ok() } else { Some(m.lock().unwrap()) }
}

/// Caller-owned buffer of an asynchronous read. The caller guarantees it stays
/// valid until the request is completed (normal overlapped I/O contract).
#[derive(Clone, Copy)]
pub struct UserBuffer {
    pub ptr: *mut u8,
    pub len: usize,
}

unsafe impl Send for UserBuffer {}

type Completion = Box<dyn FnOnce(NtStatus, usize) + Send>;

/// An asynchronous stream read (ReadFile on a pipe handle).
pub struct ReadRequest {
    buf: UserBuffer,
    file_id: u64,
    done: Option<Completion>,
}

impl ReadRequest {
    pub fn new(buf: UserBuffer, done: Completion) -> ReadRequest {
        ReadRequest { buf, file_id: 0, done: Some(done) }
    }

    pub fn len(&self) -> usize {
        self.buf.len
    }

    pub fn is_empty(&self) -> bool {
        self.buf.len == 0
    }

    pub(crate) fn complete(mut self, status: NtStatus, info: usize) {
        if let Some(f) = self.done.take() {
            f(status, info);
        }
    }
}

impl Drop for ReadRequest {
    fn drop(&mut self) {
        // Never leave an I/O hanging.
        if let Some(f) = self.done.take() {
            f(status::CANCELLED, 0);
        }
    }
}

/// An open file on the emulated device.
pub struct CameraFile {
    cam: Arc<Camera>,
    /// Configured-pipe index (None = control handle).
    pipe: Option<u8>,
    id: u64,
}

impl CameraFile {
    pub fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> (NtStatus, usize) {
        self.cam.ioctl(self.pipe, code, input, output)
    }

    pub fn read(&self, mut req: ReadRequest) {
        req.file_id = self.id;
        self.cam.read(self.pipe, req);
    }

    /// IRP_MJ_CLEANUP: cancel this file's queued reads.
    pub fn cleanup(&self) {
        self.cam.cancel_file(self.id);
    }

    pub fn camera(&self) -> &Arc<Camera> {
        &self.cam
    }
}

/// Open cameras by lower-case interface path, shared by all their files.
static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<Camera>>>> = LazyLock::new(Default::default);

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(1);

/// Parse the driver's file-name rule: the run of decimal digits ending at the last
/// digit of the name is the configured-pipe index (8-bit arithmetic).
fn parse_pipe_index(suffix: &str) -> Result<Option<u8>, NtStatus> {
    if suffix.is_empty() {
        return Ok(None);
    }
    let b = suffix.as_bytes();
    let Some(last) = b.iter().rposition(|c| c.is_ascii_digit()) else {
        return Err(status::INVALID_DEVICE_REQUEST);
    };
    let mut start = last;
    while start > 0 && b[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let mut v: u8 = 0;
    for &c in &b[start..=last] {
        v = v.wrapping_mul(10).wrapping_add(c - b'0');
    }
    Ok(Some(v))
}

/// CreateFile on `<interface path><suffix>`.
pub fn open(base_path: &str, suffix: &str) -> Result<CameraFile, NtStatus> {
    let pipe = parse_pipe_index(suffix)?;
    let key = base_path.to_ascii_lowercase();
    let cam = {
        let mut reg = REGISTRY.lock().unwrap();
        match reg.get(&key).and_then(Weak::upgrade) {
            Some(c) => c,
            None => {
                let c = Arc::new(Camera::open(base_path)?);
                reg.insert(key, Arc::downgrade(&c));
                c
            }
        }
    };
    if let Some(p) = pipe
        && p as usize >= cam.pipe_count()
    {
        return Err(status::INVALID_DEVICE_REQUEST);
    }
    Ok(CameraFile { cam, pipe, id: NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed) })
}

/// IOCTL codes (`CTL_CODE(0x22, fn, METHOD_BUFFERED, FILE_ANY_ACCESS)`).
pub mod ioctl {
    pub const GET_CONFIG_DESCRIPTOR: u32 = 0x220000;
    pub const RESET_DEVICE: u32 = 0x220004;
    pub const RESET_PIPE: u32 = 0x220008;
    pub const GET_DEPTH_TABLE_CLIPPED: u32 = 0x22000C;
    pub const GET_PACKET_STATS: u32 = 0x220010;
    pub const GET_COLOR_STATS: u32 = 0x220014;
    pub const GET_DEPTH_STATS: u32 = 0x220018;
    pub const START_DEPTH: u32 = 0x22001C;
    pub const STOP_DEPTH: u32 = 0x220020;
    pub const START_BAYER: u32 = 0x220024;
    pub const STOP_BAYER: u32 = 0x220028;
    pub const START_IR: u32 = 0x22002C;
    pub const STOP_IR: u32 = 0x220030;
    pub const NOP_34: u32 = 0x220034;
    pub const GET_FIRMWARE_VERSION: u32 = 0x220038;
    pub const RAW_COMMAND: u32 = 0x22003C;
    pub const START_YUV: u32 = 0x220040;
    pub const STOP_YUV: u32 = 0x220044;
    pub const START_HIRES_IR: u32 = 0x220048;
    pub const STOP_HIRES_IR: u32 = 0x22004C;
    pub const START_HIRES_BAYER: u32 = 0x220050;
    pub const STOP_HIRES_BAYER: u32 = 0x220054;
    pub const GET_REGISTRATION: u32 = 0x220058;
    pub const GET_FIXED_PARAMS: u32 = 0x22005C;
    pub const GET_CONST_SHIFT: u32 = 0x220060;
    pub const GET_DEPTH_TABLE_FULL: u32 = 0x220064;
    pub const NEAR_MODE_ON: u32 = 0x220068;
    pub const NEAR_MODE_OFF: u32 = 0x22006C;
    pub const CYCLE_PORT: u32 = 0x220070;
    pub const GET_RUNTIME_STATE: u32 = 0x220074;
    pub const SET_HIRES_VARIANT: u32 = 0x220078;
    pub const LASER_ON: u32 = 0x22007C;
    pub const LASER_OFF: u32 = 0x220080;
    pub const CMOS_BAYER_SETTINGS: u32 = 0x220084;
    pub const CMOS_YUV_SETTINGS: u32 = 0x220088;
    pub const UNIMPLEMENTED_8C: u32 = 0x22008C;
    pub const NUDGE_STREAM_TIMING: u32 = 0x220090;
    pub const NOP_94: u32 = 0x220094;
    pub const READ_TRACE_LOG: u32 = 0x220098;
    pub const CMOS_WRITE: u32 = 0x22009C;
    pub const CMOS_READ: u32 = 0x2200A0;
    pub const CMOS_INDIRECT_WRITE: u32 = 0x2200A4;
    pub const CMOS_INDIRECT_READ: u32 = 0x2200A8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_index_rule() {
        assert_eq!(parse_pipe_index(""), Ok(None));
        assert_eq!(parse_pipe_index("\\PIPE00"), Ok(Some(0)));
        assert_eq!(parse_pipe_index("\\PIPE01"), Ok(Some(1)));
        assert_eq!(parse_pipe_index("\\foo7bar"), Ok(Some(7)));
        assert_eq!(parse_pipe_index("\\x"), Err(status::INVALID_DEVICE_REQUEST));
    }
}
