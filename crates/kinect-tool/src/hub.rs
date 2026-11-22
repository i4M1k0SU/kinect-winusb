//! Read USB descriptors of a device through its parent hub
//! (`IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION`). This works even when the
//! device has no function driver, so it is used for diagnostics.

use windows::Win32::Devices::Usb::IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INVALID_DATA, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::{Error, HSTRING, Owned, Result};

use crate::devices::{GUID_DEVINTERFACE_USB_HUB, UsbDeviceNode, interface_paths};

pub struct HubPort {
    hub: Owned<HANDLE>,
    port: u32,
}

impl HubPort {
    pub fn for_device(node: &UsbDeviceNode) -> Result<HubPort> {
        let not_found = || Error::from_hresult(ERROR_FILE_NOT_FOUND.to_hresult());
        let parent = node.parent_instance_id.as_deref().ok_or_else(not_found)?;
        let port = node.address.ok_or_else(not_found)?;
        let path =
            interface_paths(&GUID_DEVINTERFACE_USB_HUB, Some(parent)).into_iter().next().ok_or_else(not_found)?;
        let hub = unsafe {
            Owned::new(CreateFileW(
                &HSTRING::from(path),
                GENERIC_WRITE.0,
                FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )?)
        };
        Ok(HubPort { hub, port })
    }

    /// GET_DESCRIPTOR(type, index) of `len` bytes.
    pub fn get_descriptor(&self, dtype: u8, index: u8, len: u16) -> Result<Vec<u8>> {
        const HDR: usize = 12; // ConnectionIndex + 8-byte setup packet (packed)
        let mut buf = vec![0u8; HDR + len as usize];
        buf[0..4].copy_from_slice(&self.port.to_le_bytes());
        buf[4] = 0x80; // bmRequestType: device-to-host, standard, device
        buf[5] = 6; // GET_DESCRIPTOR
        buf[6..8].copy_from_slice(&(((dtype as u16) << 8) | index as u16).to_le_bytes());
        buf[10..12].copy_from_slice(&len.to_le_bytes());
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                *self.hub,
                IOCTL_USB_GET_DESCRIPTOR_FROM_NODE_CONNECTION,
                Some(buf.as_ptr().cast()),
                buf.len() as u32,
                Some(buf.as_mut_ptr().cast()),
                buf.len() as u32,
                Some(&raw mut returned),
                None,
            )?
        };
        let n = (returned as usize).saturating_sub(HDR).min(len as usize);
        Ok(buf[HDR..HDR + n].to_vec())
    }

    pub fn device_descriptor(&self) -> Result<Vec<u8>> {
        self.get_descriptor(1, 0, 18)
    }

    pub fn config_descriptor(&self) -> Result<Vec<u8>> {
        let head = self.get_descriptor(2, 0, 9)?;
        if head.len() < 4 {
            return Err(Error::from_hresult(ERROR_INVALID_DATA.to_hresult()));
        }
        self.get_descriptor(2, 0, u16::from_le_bytes([head[2], head[3]]))
    }
}
