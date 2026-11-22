//! `install` / `uninstall`: bind the Kinect camera to Windows' inbox WinUSB
//! driver and expose the device interface class Kinect10.dll looks for.
//!
//! Only Microsoft-signed inbox components are used (`%WINDIR%\INF\winusb.inf`,
//! `winusb.sys`), so this works with Memory Integrity (HVCI) enabled and needs no
//! test signing or self-signed certificates. Requires administrator rights.

use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, HWND};
use windows::Win32::System::Registry::{REG_MULTI_SZ, RegCloseKey, RegDeleteValueW, RegSetValueExW};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::core::{BOOL, Error, HSTRING, PCWSTR, Result, w};

use crate::devices::{self, CAMERA_PIDS, GUID_KINECT_CAMERA, VID_MICROSOFT, guid_string};

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// A device information set holding one device.
struct Device {
    set: HDEVINFO,
    data: SP_DEVINFO_DATA,
}

impl Drop for Device {
    fn drop(&mut self) {
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.set) };
    }
}

impl Device {
    fn open(instance_id: &str) -> Result<Device> {
        let mut dev = Device {
            set: unsafe { SetupDiCreateDeviceInfoList(None, None)? },
            data: SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() },
        };
        unsafe { SetupDiOpenDeviceInfoW(dev.set, &HSTRING::from(instance_id), None, 0, Some(&raw mut dev.data))? };
        Ok(dev)
    }

    /// Write (or remove) `DeviceInterfaceGUIDs` in the device's hardware key
    /// ("Device Parameters").
    fn set_interface_guid(&self, enable: bool) -> Result<()> {
        unsafe {
            let key = SetupDiCreateDevRegKeyW(
                self.set,
                &raw const self.data,
                DICS_FLAG_GLOBAL.0,
                0,
                DIREG_DEV,
                None,
                PCWSTR::null(),
            )?;
            let name = w!("DeviceInterfaceGUIDs");
            let r = if enable {
                let v: Vec<[u8; 2]> =
                    guid_string(&GUID_KINECT_CAMERA).encode_utf16().chain([0, 0]).map(u16::to_le_bytes).collect();
                RegSetValueExW(key, name, None, REG_MULTI_SZ, Some(v.as_flattened()))
            } else {
                RegDeleteValueW(key, name)
            };
            let _ = RegCloseKey(key);
            if !enable && r == ERROR_FILE_NOT_FOUND {
                return Ok(());
            }
            r.ok()
        }
    }

    fn restart(&self) -> Result<()> {
        let p = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: DICS_PROPCHANGE,
            Scope: DICS_FLAG_GLOBAL,
            HwProfile: 0,
        };
        unsafe {
            SetupDiSetClassInstallParamsW(
                self.set,
                Some(&raw const self.data),
                Some(&raw const p.ClassInstallHeader),
                size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            )?;
            SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, self.set, Some(&raw const self.data))
        }
    }

    /// Install the generic "WinUsb Device" model of the inbox winusb.inf.
    /// Returns whether a reboot is required.
    fn install_inbox_winusb(&mut self) -> Result<bool> {
        let mut buf = [0u16; 260];
        let n = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
        let inf = format!("{}\\INF\\winusb.inf", from_wide(&buf[..n]));
        unsafe {
            let mut params =
                SP_DEVINSTALL_PARAMS_W { cbSize: size_of::<SP_DEVINSTALL_PARAMS_W>() as u32, ..Default::default() };
            SetupDiGetDeviceInstallParamsW(self.set, Some(&raw const self.data), &raw mut params)?;
            let mut path = [0u16; 260];
            for (d, s) in path.iter_mut().zip(inf.encode_utf16()) {
                *d = s;
            }
            params.DriverPath = path;
            params.Flags |= DI_ENUMSINGLEINF;
            params.FlagsEx |= DI_FLAGSEX_ALLOWEXCLUDEDDRVS;
            SetupDiSetDeviceInstallParamsW(self.set, Some(&raw const self.data), &raw const params)?;
            SetupDiBuildDriverInfoList(self.set, Some(&raw mut self.data), SPDIT_CLASSDRIVER)?;

            let mut chosen = None;
            for i in 0.. {
                let mut drv =
                    SP_DRVINFO_DATA_V2_W { cbSize: size_of::<SP_DRVINFO_DATA_V2_W>() as u32, ..Default::default() };
                match SetupDiEnumDriverInfoW(self.set, Some(&raw const self.data), SPDIT_CLASSDRIVER, i, &raw mut drv) {
                    Err(e) if e.code() == ERROR_NO_MORE_ITEMS.to_hresult() => break,
                    r => r?,
                }
                // Variable-length detail; u64 storage keeps it aligned.
                let mut detail = vec![0u64; 1024];
                let det = detail.as_mut_ptr().cast::<SP_DRVINFO_DETAIL_DATA_W>();
                (*det).cbSize = size_of::<SP_DRVINFO_DETAIL_DATA_W>() as u32;
                let size = (detail.len() * 8) as u32;
                let _ = SetupDiGetDriverInfoDetailW(
                    self.set,
                    Some(&raw const self.data),
                    &raw const drv,
                    Some(det),
                    size,
                    None,
                );
                let hwid = PCWSTR((&raw const (*det).HardwareID).cast()).to_string().unwrap_or_default();
                println!("  candidate driver: {} [{hwid}]", from_wide(&drv.Description));
                if hwid.eq_ignore_ascii_case(r"USB\MS_COMP_WINUSB") {
                    chosen = Some(drv);
                }
            }
            let Some(drv) = chosen else {
                eprintln!("  the generic \"WinUsb Device\" model was not found in {inf}");
                return Err(Error::from_hresult(ERROR_FILE_NOT_FOUND.to_hresult()));
            };
            println!("  installing: {}", from_wide(&drv.Description));
            let mut reboot = BOOL(0);
            DiInstallDevice(
                None,
                self.set,
                &raw const self.data,
                Some(&raw const drv),
                DIINSTALLDEVICE_FLAGS(0),
                Some(&raw mut reboot),
            )?;
            Ok(reboot.as_bool())
        }
    }
}

fn report(what: &str, e: &Error) {
    eprintln!("  {what}: {e}");
    // E_ACCESSDENIED, or SetupAPI's "no associated service".
    if matches!(e.code().0.cast_unsigned(), 0x8007_0005 | 0xE000_0228) {
        eprintln!("  (run this command from an elevated / administrator prompt)");
    }
}

fn is_winusb(c: &devices::UsbDeviceNode) -> bool {
    c.service.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("WinUSB"))
}

fn install_one(c: &devices::UsbDeviceNode) -> Result<()> {
    let mut dev = Device::open(&c.instance_id).inspect_err(|e| report("cannot open device", e))?;
    dev.set_interface_guid(true).inspect_err(|e| report("cannot write DeviceInterfaceGUIDs", e))?;
    println!("  DeviceInterfaceGUIDs = {}", guid_string(&GUID_KINECT_CAMERA));
    if is_winusb(c) {
        println!("  WinUSB already bound; restarting the device");
        if let Err(e) = dev.restart() {
            eprintln!("  restart failed: {e} (unplug and replug the Kinect)");
        }
    } else {
        let reboot = dev.install_inbox_winusb().inspect_err(|e| report("WinUSB installation failed", e))?;
        println!("  WinUSB installed{}", if reboot { " (reboot required)" } else { "" });
        let _ = dev.restart();
    }
    Ok(())
}

pub fn install(_args: &[String]) -> i32 {
    let cams = devices::usb_devices(VID_MICROSOFT, CAMERA_PIDS);
    if cams.is_empty() {
        eprintln!("no Kinect camera (045E:02AE / 045E:02BF) is connected");
        return 1;
    }
    let mut code = 0;
    for c in &cams {
        println!("camera {} (service: {})", c.instance_id, c.service.as_deref().unwrap_or("none"));
        if install_one(c).is_err() {
            code = 1;
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let paths = devices::interface_paths(&GUID_KINECT_CAMERA, None);
    println!("camera interfaces now present: {}", paths.len());
    for p in paths {
        println!("  {p}");
    }
    code
}

fn uninstall_one(c: &devices::UsbDeviceNode) -> Result<()> {
    let dev = Device::open(&c.instance_id).inspect_err(|e| report("cannot open device", e))?;
    let _ = dev.set_interface_guid(false);
    let mut reboot = BOOL(0);
    unsafe { DiUninstallDevice(HWND::default(), dev.set, &raw const dev.data, 0, Some(&raw mut reboot)) }
        .inspect_err(|e| report("uninstall failed", e))?;
    println!("  removed (the device returns without a driver after a rescan or replug)");
    let mut root = 0u32;
    unsafe {
        if CM_Locate_DevNodeW(&raw mut root, PCWSTR::null(), CM_LOCATE_DEVNODE_NORMAL) == CR_SUCCESS {
            let _ = CM_Reenumerate_DevNode(root, CM_REENUMERATE_SYNCHRONOUS);
        }
    }
    Ok(())
}

pub fn uninstall(_args: &[String]) -> i32 {
    let mut code = 0;
    for c in devices::usb_devices(VID_MICROSOFT, CAMERA_PIDS) {
        println!("camera {} (service: {})", c.instance_id, c.service.as_deref().unwrap_or("none"));
        if !is_winusb(&c) {
            println!("  not bound to WinUSB; nothing to do");
        } else if uninstall_one(&c).is_err() {
            code = 1;
        }
    }
    code
}
