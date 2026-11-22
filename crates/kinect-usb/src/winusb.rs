//! Thin wrapper over winusb.dll.

use std::ffi::c_void;

use windows::Win32::Devices::Usb::*;
use windows::Win32::Foundation::{ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::core::{HSTRING, Owned, Result};

pub struct WinUsbDevice {
    // Declared first so the interface is freed before the file handle is closed.
    iface: Owned<WINUSB_INTERFACE_HANDLE>,
    _file: Owned<HANDLE>,
}

// SAFETY: WinUSB interface handles may be used from any thread.
unsafe impl Send for WinUsbDevice {}
unsafe impl Sync for WinUsbDevice {}

#[derive(Debug, Clone, Copy)]
pub struct PipeInfo {
    pub pipe_type: USBD_PIPE_TYPE,
    pub pipe_id: u8,
    pub max_packet_size: u16,
    pub interval: u8,
    pub max_bytes_per_interval: u32,
}

impl WinUsbDevice {
    pub fn open(path: &str) -> Result<WinUsbDevice> {
        unsafe {
            let file = Owned::new(CreateFileW(
                &HSTRING::from(path),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )?);
            let mut iface = WINUSB_INTERFACE_HANDLE::default();
            WinUsb_Initialize(*file, &raw mut iface)?;
            Ok(WinUsbDevice { iface: Owned::new(iface), _file: file })
        }
    }

    /// Synchronous control transfer. Returns the number of bytes transferred.
    pub fn control(&self, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize> {
        let setup = WINUSB_SETUP_PACKET {
            RequestType: request_type,
            Request: request,
            Value: value,
            Index: index,
            Length: data.len() as u16,
        };
        let mut done = 0u32;
        let buf = (!data.is_empty()).then_some(data);
        unsafe { WinUsb_ControlTransfer(*self.iface, setup, buf, Some(&raw mut done), None)? };
        Ok(done as usize)
    }

    /// GET_DESCRIPTOR into `buf`; returns the number of bytes received.
    pub fn get_descriptor(&self, dtype: u8, index: u8, buf: &mut [u8]) -> Result<usize> {
        let mut n = 0u32;
        unsafe { WinUsb_GetDescriptor(*self.iface, dtype, index, 0, Some(buf), &raw mut n)? };
        Ok(n as usize)
    }

    pub fn query_interface(&self, alt: u8) -> Result<USB_INTERFACE_DESCRIPTOR> {
        let mut d = USB_INTERFACE_DESCRIPTOR::default();
        unsafe { WinUsb_QueryInterfaceSettings(*self.iface, alt, &raw mut d)? };
        Ok(d)
    }

    pub fn query_pipe(&self, alt: u8, index: u8) -> Result<PipeInfo> {
        let mut p = WINUSB_PIPE_INFORMATION_EX::default();
        unsafe { WinUsb_QueryPipeEx(*self.iface, alt, index, &raw mut p)? };
        Ok(PipeInfo {
            pipe_type: p.PipeType,
            pipe_id: p.PipeId,
            max_packet_size: p.MaximumPacketSize,
            interval: p.Interval,
            max_bytes_per_interval: p.MaximumBytesPerInterval,
        })
    }

    pub fn set_alternate(&self, alt: u8) -> Result<()> {
        unsafe { WinUsb_SetCurrentAlternateSetting(*self.iface, alt) }
    }

    pub fn set_pipe_policy(&self, pipe: u8, policy: WINUSB_PIPE_POLICY, value: u32) -> Result<()> {
        unsafe { WinUsb_SetPipePolicy(*self.iface, pipe, policy, 4, (&raw const value).cast()) }
    }

    pub fn reset_pipe(&self, pipe: u8) -> Result<()> {
        unsafe { WinUsb_ResetPipe(*self.iface, pipe) }
    }

    /// Register `len` bytes at `buf` for isochronous transfers on `pipe`.
    ///
    /// # Safety
    /// The buffer must stay valid until the returned handle is dropped.
    pub unsafe fn register_isoch_buffer(&self, pipe: u8, buf: *mut u8, len: usize) -> Result<IsochBuffer> {
        let mut h = std::ptr::null_mut();
        unsafe {
            let region = std::slice::from_raw_parts_mut(buf, len);
            WinUsb_RegisterIsochBuffer(*self.iface, pipe, region, &raw mut h)?;
        }
        Ok(IsochBuffer(h))
    }

    /// Get the result of an overlapped WinUSB operation.
    ///
    /// # Safety
    /// `ov` must be an OVERLAPPED previously passed to a WinUSB call on this device.
    pub unsafe fn overlapped_result(&self, ov: *const OVERLAPPED, wait: bool) -> Result<u32> {
        let mut n = 0u32;
        unsafe { WinUsb_GetOverlappedResult(*self.iface, ov, &raw mut n, wait)? };
        Ok(n)
    }
}

/// Registered isochronous buffer handle (unregistered on drop).
pub struct IsochBuffer(*mut c_void);

impl IsochBuffer {
    /// Queue an ASAP isochronous IN transfer of `descs.len()` packets into
    /// `[offset, offset+len)` of the registered buffer.
    ///
    /// # Safety
    /// `descs` and `ov` must stay valid until the transfer completes.
    pub unsafe fn read_asap(
        &self,
        offset: u32,
        len: u32,
        continue_stream: bool,
        descs: &mut [USBD_ISO_PACKET_DESCRIPTOR],
        ov: *const OVERLAPPED,
    ) -> Result<()> {
        match unsafe { WinUsb_ReadIsochPipeAsap(self.0, offset, len, continue_stream, descs, Some(ov)) } {
            Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => Err(e),
            _ => Ok(()),
        }
    }
}

impl Drop for IsochBuffer {
    fn drop(&mut self) {
        if let Err(e) = unsafe { WinUsb_UnregisterIsochBuffer(self.0) } {
            log::error!("WinUsb_UnregisterIsochBuffer failed: {e}");
        }
    }
}
