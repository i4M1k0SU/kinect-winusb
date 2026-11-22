//! PnP enumeration (CfgMgr32).

use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Devices::Properties::*;
use windows::Win32::Foundation::DEVPROPKEY;
use windows::core::{GUID, HSTRING, PCWSTR};

/// Device interface class registered by the official `kinectcamera.sys`.
/// `Kinect10.dll` looks the camera up through this interface class, so the
/// WinUSB-bound camera must expose it too (see `install`).
pub const GUID_KINECT_CAMERA: GUID = GUID::from_u128(0x00873FDF_61A8_11D1_EF5E_00C04F2D728B);
/// Interface class of the Kinect "Device" (motor / hub controller) WinUSB function.
pub const GUID_KINECT_DEVICE: GUID = GUID::from_u128(0xE4CFCB17_3651_4BDA_8607_528812E60503);
pub const GUID_DEVINTERFACE_USB_HUB: GUID = GUID::from_u128(0xF18A0E88_C30C_11D0_8815_00A0C906BED8);

pub const VID_MICROSOFT: u16 = 0x045E;
/// Camera PIDs handled by kinectcamera.inf (Xbox 360 Kinect / Kinect for Windows).
pub const CAMERA_PIDS: &[u16] = &[0x02AE, 0x02BF];

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`
pub fn guid_string(g: &GUID) -> String {
    format!("{{{g:?}}}")
}

/// Parse `VID_xxxx` / `PID_xxxx` out of a device instance ID (case-insensitive).
pub fn parse_vid_pid(s: &str) -> Option<(u16, u16)> {
    let up = s.to_ascii_uppercase();
    let grab = |tag: &str| -> Option<u16> {
        let i = up.find(tag)? + tag.len();
        u16::from_str_radix(up.get(i..i + 4)?, 16).ok()
    };
    Some((grab("VID_")?, grab("PID_")?))
}

fn multi_sz(buf: &[u16]) -> Vec<String> {
    buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
}

/// Paths of the present device interfaces of class `guid`, optionally only
/// those of one device instance.
pub fn interface_paths(guid: &GUID, instance_id: Option<&str>) -> Vec<String> {
    let id = instance_id.map(HSTRING::from);
    let id = id.as_ref().map_or(PCWSTR::null(), |h| PCWSTR(h.as_ptr()));
    let flags = CM_GET_DEVICE_INTERFACE_LIST_PRESENT;
    let mut len = 0u32;
    unsafe {
        if CM_Get_Device_Interface_List_SizeW(&raw mut len, guid, id, flags) != CR_SUCCESS {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(guid, id, &mut buf, flags) != CR_SUCCESS {
            return Vec::new();
        }
        multi_sz(&buf)
    }
}

/// Device instance ID that owns a device interface path.
pub fn interface_instance_id(path: &str) -> Option<String> {
    let mut buf = [0u8; 1024];
    let mut ty = DEVPROPTYPE::default();
    let mut size = buf.len() as u32;
    let r = unsafe {
        CM_Get_Device_Interface_PropertyW(
            &HSTRING::from(path),
            &DEVPKEY_Device_InstanceId,
            &raw mut ty,
            Some(buf.as_mut_ptr()),
            &raw mut size,
            0,
        )
    };
    (r == CR_SUCCESS).then(|| utf16_bytes(&buf[..size as usize]))
}

/// A NUL-terminated UTF-16LE string in a property buffer.
fn utf16_bytes(b: &[u8]) -> String {
    let len = b.as_chunks::<2>().0.iter().position(|&c| c == [0, 0]).map_or(b.len() & !1, |i| 2 * i);
    String::from_utf16le_lossy(&b[..len])
}

fn property(dev: u32, key: &DEVPROPKEY, buf: &mut [u8]) -> Option<usize> {
    let mut ty = DEVPROPTYPE::default();
    let mut size = buf.len() as u32;
    let r = unsafe { CM_Get_DevNode_PropertyW(dev, key, &raw mut ty, Some(buf.as_mut_ptr()), &raw mut size, 0) };
    (r == CR_SUCCESS).then_some(size as usize)
}

fn property_string(dev: u32, key: &DEVPROPKEY) -> Option<String> {
    let mut buf = [0u8; 1024];
    let n = property(dev, key, &mut buf)?;
    Some(utf16_bytes(&buf[..n]))
}

fn property_u32(dev: u32, key: &DEVPROPKEY) -> Option<u32> {
    let mut buf = [0u8; 4];
    property(dev, key, &mut buf)?;
    Some(u32::from_le_bytes(buf))
}

/// Summary of a present USB device node.
#[derive(Debug, Clone)]
pub struct UsbDeviceNode {
    pub instance_id: String,
    pub vid: u16,
    pub pid: u16,
    pub description: Option<String>,
    pub service: Option<String>,
    /// USB port number on the parent hub.
    pub address: Option<u32>,
    pub parent_instance_id: Option<String>,
    pub problem: Option<u32>,
}

/// Present USB device nodes with the given VID (and PID if `pids` is non-empty).
pub fn usb_devices(vid: u16, pids: &[u16]) -> Vec<UsbDeviceNode> {
    let usb = HSTRING::from("USB");
    let flags = CM_GETIDLIST_FILTER_ENUMERATOR | CM_GETIDLIST_FILTER_PRESENT;
    let mut len = 0u32;
    let mut buf;
    unsafe {
        if CM_Get_Device_ID_List_SizeW(&raw mut len, &usb, flags) != CR_SUCCESS {
            return Vec::new();
        }
        buf = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(&usb, &mut buf, flags) != CR_SUCCESS {
            return Vec::new();
        }
    }
    multi_sz(&buf)
        .into_iter()
        .filter_map(|id| {
            let (v, p) = parse_vid_pid(&id)?;
            if v != vid || (!pids.is_empty() && !pids.contains(&p)) {
                return None;
            }
            let mut dev = 0u32;
            let r = unsafe { CM_Locate_DevNodeW(&raw mut dev, &HSTRING::from(&id), CM_LOCATE_DEVNODE_NORMAL) };
            if r != CR_SUCCESS {
                return None;
            }
            let (mut st, mut problem) = (CM_DEVNODE_STATUS_FLAGS(0), CM_PROB(0));
            let status_ok = unsafe { CM_Get_DevNode_Status(&raw mut st, &raw mut problem, dev, 0) } == CR_SUCCESS;
            Some(UsbDeviceNode {
                vid: v,
                pid: p,
                description: property_string(dev, &DEVPKEY_Device_DeviceDesc),
                service: property_string(dev, &DEVPKEY_Device_Service),
                address: property_u32(dev, &DEVPKEY_Device_Address),
                parent_instance_id: property_string(dev, &DEVPKEY_Device_Parent),
                problem: status_ok.then_some(problem.0),
                instance_id: id,
            })
        })
        .collect()
}
