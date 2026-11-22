//! User-mode re-implementation of the Kinect v1 camera driver on top of WinUSB.
//!
//! The official Kinect for Windows runtime (`Kinect10.dll`) talks to the camera
//! through the KMDF kernel driver `kinectcamera.sys`, which cannot be loaded when
//! Memory Integrity (HVCI) is enabled. This crate provides the same externally
//! visible behaviour (IOCTLs and stream reads) in user mode so that a proxy
//! `Kinect10.dll` can redirect the official runtime to it.

use std::fmt;

pub mod camera;
pub mod logger;
pub mod winusb;

#[cfg_attr(target_arch = "x86", link(name = "ntdll", kind = "raw-dylib", import_name_type = "undecorated"))]
#[cfg_attr(not(target_arch = "x86"), link(name = "ntdll", kind = "raw-dylib"))]
unsafe extern "system" {
    safe fn RtlDllShutdownInProgress() -> u8;
}

/// Whether the process is exiting: `ExitProcess` has terminated every other
/// thread and is running the DLLs' `DLL_PROCESS_DETACH` code. A terminated thread
/// may have held any lock, and threads can no longer be joined.
pub fn process_exiting() -> bool {
    RtlDllShutdownInProgress() != 0
}

/// Display bytes as space-separated hex (for logs).
pub fn hex(bytes: &[u8]) -> impl fmt::Display {
    fmt::from_fn(move |f| {
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    })
}
