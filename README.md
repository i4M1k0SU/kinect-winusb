English | [日本語](README.ja.md)

# Kinect WinUSB

A compatibility runtime for using Kinect v1 (Kinect for Xbox 360 / Kinect for Windows) **with Core isolation's Memory integrity (HVCI) turned on**.

Just place `kinect10.dll` in an application's folder, and apps built with the Kinect for Windows SDK v1.x (v1.0 – v1.8) work: native or .NET, 64-bit or 32-bit.

The target is **64-bit Windows 10 / 11**. Memory integrity exists only there; earlier versions and 32-bit editions can use the official driver as is, so this project does not support them.


## Requirements

- Windows 10 (x64) / Windows 11
- [Kinect for Windows Runtime v1.8](https://www.microsoft.com/en-us/download/details.aspx?id=40277) installed
  - The v1.5 – v1.7 runtimes are supported but untested on real hardware
  - The v1.0 runtime is not supported
  - v1.8 is backward compatible, so use it unless you have a particular reason not to
- Kinect v1
  - Kinect for Windows
  - Kinect for Xbox 360

## Usage

1. Change the Kinect's driver
   - Switch it to the WinUSB driver with Zadig or Device Manager
   - The `kinect-winusb install` command also does it (requires administrator rights)
1. Copy the `kinect10.dll` that matches the executable's bitness into the same folder as the app's or game's executable

### If you already use Kinect v1 with Memory integrity turned off

1. Turn on Memory integrity in the Windows Security dashboard
1. The Kinect driver is reported as an incompatible driver
1. Check the driver's published name (`oemXX.inf`) and note it down
1. Run Command Prompt as administrator
1. Remove the driver with the following command
   - `pnputil /delete-driver oemXX.inf /uninstall /force` (replace `oemXX.inf` with the name you noted)
1. Turn on Memory integrity again
   - If other drivers were reported too, it cannot be turned on until you deal with each of them

## For developers

### Crates

| Crate | Contents |
|---|---|
| `crates/kinect10` | Proxy Kinect10.dll (export stubs, loading the official DLL, IAT hooks) |
| `crates/kinect-usb` | User-mode kinectcamera.sys (WinUSB, protocol, frame assembly, logger) |
| `crates/kinect-tool` | kinect-winusb.exe (install / uninstall / probe / camtest / nuitest, PnP enumeration, descriptor reads through the hub) |

### Build

```bash
cargo build --release
cargo build --release --target i686-pc-windows-msvc
```

### Diagnostic commands

```bash
# Show the Kinect's USB functions, their assigned drivers and the camera's descriptors. Reads through the hub, so it works even with no driver assigned
kinect-winusb probe

# Receive depth and color with the user-mode driver alone, without the official DLL. Writes `depth.pgm` and `color_bayer.pgm`
kinect-winusb camtest 90

# Run the NUI API through the `kinect10.dll` (proxy) next to the exe. Writes `nui_color.ppm` and `nui_depth.pgm`. With `--no-shutdown` it exits without calling `NuiShutdown`, reproducing an app that exits without cleaning up
kinect-winusb nuitest --seconds 10
```

### Tests for .NET (`Microsoft.Kinect.dll`)

Build instructions are at the top of each file.

| File | Contents |
|---|---|
| `tests/managed/KinectSmoke.cs` | A short test that only checks that color, depth and skeleton data arrive |
| `tests/managed/KinectFeatures.cs` | Checks every color format, every depth resolution, skeleton modes, coordinate mapping, tilt, audio, and stop and restart in turn, printing `PASS` / `FAIL` / `INFO`. `hotplug <seconds>` also checks unplugging and replugging |

Logging is controlled by the following environment variables.

| Variable | Meaning |
|---|---|
| `KINECT_WINUSB_LOG=<path>` | Append the log to a file |
| `KINECT_WINUSB_LOG_LEVEL=0..3` | 0 = errors, 1 = info (default), 2 = IOCTLs, 3 = all I/O |

Messages at level 1 and below also go to `OutputDebugString`.

### How it works

Of the Kinect for Windows Runtime v1.8 drivers, Device (motor), Audio Array and Security already run on WinUSB. **Only the camera's `kinectcamera.sys` (KMDF)** needs a kernel driver. That driver is incompatible with Memory integrity, so the camera alone stays unusable as "Xbox NUI Camera (Code 28)".

This project reimplements what `kinectcamera.sys` does in user mode and reroutes the official runtime to it.
Skeleton tracking, player indices, coordinate mapping and so on are still handled by the official `Kinect10.dll` as is. Only the part that corresponds to the camera driver is replaced.

```
App (NUI API / Microsoft.Kinect.dll)
   │  LoadLibrary("Kinect10.dll")
   ▼
kinect10.dll (this project's proxy)
   │  Passes every export on to the official DLL through a tiny stub with the same name and ordinal (a single jmp)
   │  DllMain loads the official DLL and rewrites only its imports (IAT hooks)
   ▼
Official Kinect10.dll (System32 / SysWOW64) ... skeleton tracking and the rest stay official
   │  Hooked: CreateFileA/W, DeviceIoControl, ReadFile, CloseHandle,
   │          CreateIoCompletionPort, GetOverlappedResult
   ▼ Only I/O to the camera ({00873FDF-61A8-11D1-EF5E-00C04F2D728B}) is intercepted
kinect-usb (user-mode kinectcamera.sys)
   │  Camera command protocol, about 40 IOCTLs, isochronous transfers → frame assembly
   ▼
WinUSB (the inbox, Microsoft-signed winusb.sys) ── Kinect camera
```

### Differences from the official driver

- **Stopping a stream**: `WinUsb_AbortPipe` is not used, because aborting an isochronous pipe makes every later transfer fail with `ERROR_BUSY`. Queued transfers are left to finish on their own instead, so stopping takes up to about 100 ms
- **First open**: the first time a process opens the camera, it sends the stream-stop commands as a precaution (to stop streams left running by a previous process that crashed)
- **Close**: closing a handle also cancels the reads in progress on that handle, so nothing is written into their buffers after it is closed
- **Process exit**: when an app exits without calling `NuiShutdown`, the official runtime sends the stream stops and laser off while its DLL unloads (`DLL_PROCESS_DETACH`)
- **Cycle port (0x220070)**: not possible with WinUSB, so it does nothing and returns success

## License

[MIT LICENSE](LICENSE)
