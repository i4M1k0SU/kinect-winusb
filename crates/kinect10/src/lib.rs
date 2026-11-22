//! Drop-in `Kinect10.dll` for the Kinect for Windows Runtime v1.x.
//!
//! Place this DLL next to an application that uses the Kinect v1 SDK. It exports
//! every function of the official `Kinect10.dll` under the same name and ordinal;
//! each export jumps to the official function in the system directory. `DllMain`
//! loads that DLL and patches its imports so that its camera I/O (normally served
//! by the `kinectcamera.sys` kernel driver) reaches the user-mode WinUSB
//! implementation instead.

mod hooks;
mod iat;

use std::ffi::{CStr, c_void};
use std::sync::atomic::{AtomicPtr, Ordering};

use windows_sys::Win32::Foundation::{E_NOTIMPL, HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::core::{BOOL, HRESULT};

/// The official runtime. `\\.\GLOBALROOT\SystemRoot` avoids hard-coding the
/// Windows drive. Only 64-bit Windows 10+ is supported (Memory Integrity exists
/// nowhere else), so a 32-bit build always runs under WOW64 and uses the
/// SysWOW64 copy.
const OFFICIAL: &str = cfg_select! {
    target_pointer_width = "64" => r"\\.\GLOBALROOT\SystemRoot\System32\Kinect10.dll",
    target_pointer_width = "32" => r"\\.\GLOBALROOT\SystemRoot\SysWOW64\Kinect10.dll",
};

/// What an export returns when the installed runtime lacks its function.
trait Missing {
    const VALUE: Self;
}
impl Missing for HRESULT {
    const VALUE: Self = E_NOTIMPL;
}
impl Missing for () {
    const VALUE: Self = ();
}
impl Missing for *const c_void {
    const VALUE: Self = std::ptr::null();
}

struct Export {
    ordinal: u16,
    name: &'static CStr,
    /// Look the function up by ordinal (it has no name).
    by_ordinal: bool,
    /// Jump target of the export's thunk.
    slot: &'static AtomicPtr<c_void>,
}

// `arch!`: the thunk symbol name and the thunk's jump. Linker directives take
// symbol names literally, and x86 C symbols carry a leading underscore.
cfg_select! {
    target_arch = "x86_64" => {
        macro_rules! arch {
            (symbol $name:ident) => {
                concat!("kinect10_", stringify!($name))
            };
            (jump) => {
                "jmp qword ptr [rip + {slot}]"
            };
        }
    }
    target_arch = "x86" => {
        macro_rules! arch {
            (symbol $name:ident) => {
                concat!("_kinect10_", stringify!($name))
            };
            (jump) => {
                "jmp dword ptr [{slot}]"
            };
        }
    }
}

/// Define the exports. Each slot starts at a fallback with the function's own
/// signature that returns [`Missing::VALUE`], and [`resolve`] points it at the
/// official function. So a function that the installed runtime lacks (older
/// runtimes predate some) fails cleanly instead of keeping the application from
/// loading. Every parameter of these functions is an integer, handle or pointer,
/// so the fallbacks take machine words; the counts match the stack sizes of the
/// official x86 stdcall exports, which the fallbacks must pop (listed in
/// `docs/runtime-versions.md`).
macro_rules! exports {
    ($($ord:literal $([$noname:ident])? $name:ident($($arg:ident),*) -> $ret:ty;)*) => {
        $(
            #[allow(non_snake_case)]
            mod $name {
                use super::*;

                #[allow(unused_variables)]
                unsafe extern "system" fn missing($($arg: usize),*) -> $ret {
                    <$ret as Missing>::VALUE
                }

                pub static SLOT: AtomicPtr<c_void> = AtomicPtr::new(missing as *mut c_void);

                // The export itself: `jmp [SLOT]`. It is written in assembly
                // because rustc exports every `#[no_mangle]` function by itself,
                // without ordinals; the `/EXPORT` directives below export it.
                std::arch::global_asm!(
                    concat!(".globl ", arch!(symbol $name)),
                    ".p2align 4",
                    concat!(arch!(symbol $name), ":"),
                    arch!(jump),
                    slot = sym SLOT,
                );
            }
        )*

        const EXPORTS: &[Export] = &[$(Export {
            ordinal: $ord,
            name: match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
                Ok(s) => s,
                Err(_) => unreachable!(),
            },
            by_ordinal: !stringify!($($noname)?).is_empty(),
            slot: &$name::SLOT,
        }),*];

        const DIRECTIVES: &str = concat!($(
            " /EXPORT:", stringify!($name), "=", arch!(symbol $name), ",@", $ord
            $(, ",", stringify!($noname))?
        ),*);
        #[used]
        #[unsafe(link_section = ".drectve")]
        static LINKER_DIRECTIVES: [u8; DIRECTIVES.len()] = *DIRECTIVES.as_bytes().as_array().unwrap();
    };
}

exports! {
    // Ordinal 1 has no name; it returns a pointer to a static object.
    1 [NONAME] ordinal_1() -> *const c_void;
    5 NuiGetSensorCount(count) -> HRESULT;
    6 NuiCreateSensorByIndex(index, sensor) -> HRESULT;
    7 NuiCameraElevationGetAngle(angle) -> HRESULT;
    8 NuiCameraElevationSetAngle(angle) -> HRESULT;
    9 NuiImageGetColorPixelCoordinatesFromDepthPixel(res, view, x, y, depth, cx, cy) -> HRESULT;
    10 NuiImageGetColorPixelCoordinatesFromDepthPixelAtResolution(cres, dres, view, x, y, depth, cx, cy) -> HRESULT;
    11 NuiImageStreamGetNextFrame(stream, timeout, frame) -> HRESULT;
    12 NuiImageStreamOpen(kind, res, flags, limit, event, stream) -> HRESULT;
    13 NuiImageStreamReleaseFrame(stream, frame) -> HRESULT;
    14 NuiInitialize(flags) -> HRESULT;
    15 NuiSetFrameEndEvent(event, flags) -> HRESULT;
    16 NuiShutdown() -> ();
    17 NuiSkeletonGetNextFrame(timeout, frame) -> HRESULT;
    18 NuiSkeletonTrackingDisable() -> HRESULT;
    19 NuiSkeletonTrackingEnable(event, flags) -> HRESULT;
    20 NuiTransformSmooth(frame, params) -> HRESULT;
    21 NuiCreateSensorById(id, sensor) -> HRESULT;
    22 NuiSetDeviceStatusCallback(callback, user) -> ();
    23 NuiImageStreamSetImageFrameFlags(stream, flags) -> HRESULT;
    24 NuiImageStreamGetImageFrameFlags(stream, flags) -> HRESULT;
    25 NuiGetAudioSource(source) -> HRESULT;
    26 NuiSkeletonSetTrackedSkeletons(ids) -> HRESULT;
    27 NuiSkeletonCalculateBoneOrientations(skeleton, orientations) -> HRESULT;
    28 NuiCreateCoordinateMapperFromParameters(size, data, mapper) -> HRESULT;
    29 NuiCreateDepthFilter(file, index, filter) -> HRESULT;
}

/// Point the export slots at the official runtime's functions. Named functions
/// are looked up by name: runtime v1.0 used ordinals 26–29 for its COM entry
/// points (`DllCanUnloadNow` and so on), which later versions reassigned.
fn resolve(official: HMODULE) {
    for e in EXPORTS {
        // An ordinal goes in place of the name as a small integer (MAKEINTRESOURCE).
        let key = if e.by_ordinal { std::ptr::without_provenance(e.ordinal.into()) } else { e.name.as_ptr().cast() };
        match unsafe { GetProcAddress(official, key) } {
            Some(f) => e.slot.store(f as *mut c_void, Ordering::Release),
            None if e.by_ordinal => log::warn!("the official runtime has no ordinal {}", e.ordinal),
            None => log::warn!("the official runtime has no {:?}; it will fail with E_NOTIMPL", e.name),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn module_path(h: HMODULE) -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(h, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        kinect_usb::logger::init(false);
        log::info!(
            "proxy {} v{} ({}-bit) loaded in {}",
            module_path(module),
            env!("CARGO_PKG_VERSION"),
            usize::BITS,
            module_path(std::ptr::null_mut())
        );
        // Redirect the official runtime's camera I/O before anything calls it.
        let h = unsafe { LoadLibraryW(wide(OFFICIAL).as_ptr()) };
        if h.is_null() {
            log::error!(
                "cannot load the official Kinect runtime {OFFICIAL} (install the Kinect for Windows Runtime); every function will fail"
            );
        } else {
            log::info!("official runtime: {}", module_path(h));
            unsafe { hooks::install(h) };
            resolve(h);
        }
    }
    TRUE
}

#[cfg(test)]
mod tests {
    use super::*;

    // Thunks of functions without parameters, where cdecl and stdcall agree.
    unsafe extern "C" {
        #[link_name = "kinect10_NuiSkeletonTrackingDisable"]
        fn thunk_tracking_disable() -> HRESULT;
        #[link_name = "kinect10_ordinal_1"]
        fn thunk_ordinal_1() -> *const c_void;
    }

    #[test]
    fn unresolved_exports_fail_cleanly() {
        // No runtime is resolved in the test process: the thunks reach the fallbacks.
        assert_eq!(unsafe { thunk_tracking_disable() }, E_NOTIMPL);
        assert!(unsafe { thunk_ordinal_1() }.is_null());
        let init: unsafe extern "system" fn(usize) -> HRESULT =
            unsafe { std::mem::transmute(NuiInitialize::SLOT.load(Ordering::Acquire)) };
        assert_eq!(unsafe { init(0) }, E_NOTIMPL);
        type At = unsafe extern "system" fn(usize, usize, usize, usize, usize, usize, usize, usize) -> HRESULT;
        let at: At = unsafe {
            std::mem::transmute(
                NuiImageGetColorPixelCoordinatesFromDepthPixelAtResolution::SLOT.load(Ordering::Acquire),
            )
        };
        assert_eq!(unsafe { at(1, 2, 0, 3, 4, 5, 0, 0) }, E_NOTIMPL);
        let shutdown: unsafe extern "system" fn() =
            unsafe { std::mem::transmute(NuiShutdown::SLOT.load(Ordering::Acquire)) };
        unsafe { shutdown() };
    }

    #[test]
    fn export_table() {
        assert_eq!(EXPORTS.len(), 26);
        assert!(EXPORTS[0].by_ordinal && EXPORTS[0].ordinal == 1);
        assert!(EXPORTS[1..].iter().all(|e| !e.by_ordinal));
        assert_eq!(EXPORTS[25].name, c"NuiCreateDepthFilter");
        assert!(DIRECTIVES.starts_with(concat!(
            " /EXPORT:ordinal_1=",
            arch!(symbol ordinal_1),
            ",@1,NONAME /EXPORT:NuiGetSensorCount=",
            arch!(symbol NuiGetSensorCount),
            ",@5 "
        )));
    }
}
