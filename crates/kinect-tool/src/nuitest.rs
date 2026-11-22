//! `nuitest`: drive the Kinect v1 NUI API through a Kinect10.dll (normally the
//! proxy built from this repository) and report what comes back.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use std::ffi::CStr;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects};

use windows::core::{HSTRING, PCSTR};

const NUI_INITIALIZE_FLAG_USES_DEPTH_AND_PLAYER_INDEX: u32 = 0x1;
const NUI_INITIALIZE_FLAG_USES_COLOR: u32 = 0x2;
const NUI_INITIALIZE_FLAG_USES_SKELETON: u32 = 0x8;
const NUI_IMAGE_TYPE_DEPTH_AND_PLAYER_INDEX: u32 = 0;
const NUI_IMAGE_TYPE_COLOR: u32 = 1;
const NUI_IMAGE_RESOLUTION_640X480: u32 = 2;
const NUI_IMAGE_RESOLUTION_320X240: u32 = 1;

#[repr(C)]
struct NuiImageFrame {
    timestamp: i64,
    frame_number: u32,
    image_type: u32,
    resolution: u32,
    texture: *mut *const TextureVtbl,
    flags: u32,
    view_area: [i32; 3],
}

#[repr(C)]
struct LockedRect {
    pitch: i32,
    size: i32,
    bits: *mut u8,
}

#[repr(C)]
struct TextureVtbl {
    query_interface: usize,
    add_ref: usize,
    release: usize,
    buffer_len: unsafe extern "system" fn(*mut c_void) -> i32,
    pitch: unsafe extern "system" fn(*mut c_void) -> i32,
    lock_rect: unsafe extern "system" fn(*mut c_void, u32, *mut LockedRect, *mut c_void, u32) -> i32,
    get_level_desc: usize,
    unlock_rect: unsafe extern "system" fn(*mut c_void, u32) -> i32,
}

type FnInit = unsafe extern "system" fn(u32) -> i32;
type FnVoid = unsafe extern "system" fn();
type FnCount = unsafe extern "system" fn(*mut i32) -> i32;
type FnOpen = unsafe extern "system" fn(u32, u32, u32, u32, HANDLE, *mut HANDLE) -> i32;
type FnNext = unsafe extern "system" fn(HANDLE, u32, *mut *const NuiImageFrame) -> i32;
type FnRelease = unsafe extern "system" fn(HANDLE, *const NuiImageFrame) -> i32;
type FnSkelEnable = unsafe extern "system" fn(HANDLE, u32) -> i32;
type FnSkelNext = unsafe extern "system" fn(u32, *mut u8) -> i32;
type FnAngle = unsafe extern "system" fn(*mut i32) -> i32;

unsafe fn sym<T>(h: HMODULE, name: &CStr) -> T {
    let p =
        unsafe { GetProcAddress(h, PCSTR(name.as_ptr().cast())) }.unwrap_or_else(|| panic!("export {name:?} missing"));
    unsafe { std::mem::transmute_copy(&p) }
}

fn save_pgm(path: &str, w: usize, h: usize, px: impl Iterator<Item = u8>) {
    let mut data = format!("P5\n{w} {h}\n255\n").into_bytes();
    data.extend(px.take(w * h));
    if std::fs::write(path, data).is_ok() {
        println!("  wrote {path}");
    }
}

fn save_ppm_bgra(path: &str, w: usize, h: usize, bgra: &[u8]) {
    let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
    for &[b, g, r, _] in bgra.as_chunks().0.iter().take(w * h) {
        data.extend_from_slice(&[r, g, b]);
    }
    if std::fs::write(path, data).is_ok() {
        println!("  wrote {path}");
    }
}

/// Depth as gray with player pixels tinted by player index.
fn save_ppm_players(path: &str, w: usize, h: usize, depth: &[u16]) {
    const TINT: [[u8; 3]; 7] =
        [[255, 64, 64], [64, 255, 64], [64, 128, 255], [255, 255, 64], [255, 64, 255], [64, 255, 255], [255, 160, 64]];
    let mut data = format!(
        "P6
{w} {h}
255
"
    )
    .into_bytes();
    for &v in depth.iter().take(w * h) {
        let mm = (v >> 3) as u32;
        let g = if mm == 0 { 0 } else { (255 - mm.min(4000) * 255 / 4000).max(1) } as u8;
        let p = (v & 7) as usize;
        if p == 0 {
            data.extend_from_slice(&[g, g, g]);
        } else {
            let t = TINT[p - 1];
            let mix = |c: u8| c.midpoint(g);
            data.extend_from_slice(&[mix(t[0]), mix(t[1]), mix(t[2])]);
        }
    }
    if std::fs::write(path, data).is_ok() {
        println!("  wrote {path}");
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut dll = None;
    let mut seconds = 10u64;
    // Exit with the streams still running, like an application that never calls NuiShutdown.
    let mut no_shutdown = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dll" => dll = it.next().cloned(),
            "--seconds" => seconds = it.next().and_then(|s| s.parse().ok()).unwrap_or(10),
            "--no-shutdown" => no_shutdown = true,
            _ => {}
        }
    }
    let dll = dll.unwrap_or_else(|| {
        let exe = std::env::current_exe().unwrap();
        exe.with_file_name("kinect10.dll").to_string_lossy().into_owned()
    });
    println!("loading {dll}");
    let h = match unsafe { LoadLibraryW(&HSTRING::from(&dll)) } {
        Ok(h) => h,
        Err(e) => {
            eprintln!("LoadLibrary failed: {e}");
            return 1;
        }
    };
    unsafe {
        let get_count: FnCount = sym(h, c"NuiGetSensorCount");
        let init: FnInit = sym(h, c"NuiInitialize");
        let shutdown: FnVoid = sym(h, c"NuiShutdown");
        let open: FnOpen = sym(h, c"NuiImageStreamOpen");
        let next: FnNext = sym(h, c"NuiImageStreamGetNextFrame");
        let release: FnRelease = sym(h, c"NuiImageStreamReleaseFrame");
        let skel_enable: FnSkelEnable = sym(h, c"NuiSkeletonTrackingEnable");
        let skel_next: FnSkelNext = sym(h, c"NuiSkeletonGetNextFrame");
        let get_angle: FnAngle = sym(h, c"NuiCameraElevationGetAngle");

        let mut n = 0;
        let hr = get_count(&raw mut n);
        println!("NuiGetSensorCount -> 0x{:08X}, count {n}", hr.cast_unsigned());
        let flags = NUI_INITIALIZE_FLAG_USES_DEPTH_AND_PLAYER_INDEX
            | NUI_INITIALIZE_FLAG_USES_COLOR
            | NUI_INITIALIZE_FLAG_USES_SKELETON;
        let hr = init(flags);
        println!("NuiInitialize(0x{flags:x}) -> 0x{:08X}", hr.cast_unsigned());
        if hr < 0 {
            return 1;
        }
        let mut angle = 0;
        let hr = get_angle(&raw mut angle);
        println!("NuiCameraElevationGetAngle -> 0x{:08X}, {angle} deg", hr.cast_unsigned());

        // Manual-reset events, as in the SDK samples: GetNextFrame waits on them too.
        let event = || CreateEventW(None, true, false, None).expect("CreateEvent");
        let (ev_color, ev_depth, ev_skel) = (event(), event(), event());
        let mut s_color = HANDLE::default();
        let mut s_depth = HANDLE::default();
        let hr = open(NUI_IMAGE_TYPE_COLOR, NUI_IMAGE_RESOLUTION_640X480, 0, 2, ev_color, &raw mut s_color);
        println!("NuiImageStreamOpen(color 640x480) -> 0x{:08X}", hr.cast_unsigned());
        let hr =
            open(NUI_IMAGE_TYPE_DEPTH_AND_PLAYER_INDEX, NUI_IMAGE_RESOLUTION_320X240, 0, 2, ev_depth, &raw mut s_depth);
        println!("NuiImageStreamOpen(depth+player 320x240) -> 0x{:08X}", hr.cast_unsigned());
        let hr = skel_enable(ev_skel, 0);
        println!("NuiSkeletonTrackingEnable -> 0x{:08X}", hr.cast_unsigned());

        let start = Instant::now();
        let mut counts = [0u32; 3];
        let mut tracked_max = 0;
        let mut last_color: Option<Vec<u8>> = None;
        let mut last_depth: Option<Vec<u16>> = None;
        let events = [ev_color, ev_depth, ev_skel];
        let mut skel = vec![0u8; 8192];
        let mut waits = [0u32; 4];
        // Per-second report and the snapshot with the most player pixels.
        let mut next_report = Duration::from_secs(1);
        let mut tracked_now = 0usize;
        let mut head: Option<[f32; 3]> = None;
        let mut tracked_frames = 0u32;
        let mut players_now = 0usize;
        let mut best: Option<(usize, Vec<u16>, Option<Vec<u8>>)> = None;
        while start.elapsed() < Duration::from_secs(seconds) {
            if start.elapsed() >= next_report {
                let h = head
                    .map(|p| format!("head x={:+.2} y={:+.2} z={:.2} m", p[0], p[1], p[2]))
                    .unwrap_or_else(|| "-".into());
                println!("  [{:>2}s] tracked {tracked_now}, player pixels {players_now}, {h}", next_report.as_secs());
                next_report += Duration::from_secs(1);
            }
            let r = WaitForMultipleObjects(&events, false, 1000).0;
            waits[(r as usize).min(3)] += 1;
            if r > 2 {
                continue;
            }
            let i = r as usize;
            if i == 2 {
                let shr = skel_next(0, skel.as_mut_ptr());
                if shr < 0 && counts[2] == 0 {
                    println!("  skeleton: 0x{:08X}", shr.cast_unsigned());
                }
                if shr >= 0 {
                    counts[2] += 1;
                    // NUI_SKELETON_FRAME: 48-byte header, then 6 x 436-byte skeletons.
                    // NUI_SKELETON_DATA: state @0, position @16, joints @32 (20 x Vector4).
                    let f32_at = |o: usize| f32::from_le_bytes(*skel[o..o + 4].as_array().unwrap());
                    let tracked: Vec<usize> = (0..6)
                        .filter(|k| {
                            let o = 48 + k * 436;
                            u32::from_le_bytes(*skel[o..o + 4].as_array().unwrap()) == 2
                        })
                        .collect();
                    tracked_now = tracked.len();
                    tracked_max = tracked_max.max(tracked.len());
                    if tracked.is_empty() {
                        head = None;
                    } else {
                        tracked_frames += 1;
                        let o = 48 + tracked[0] * 436 + 32 + 3 * 16; // NUI_SKELETON_POSITION_HEAD
                        head = Some([f32_at(o), f32_at(o + 4), f32_at(o + 8)]);
                    }
                }
                continue;
            }
            let stream = if i == 0 { s_color } else { s_depth };
            let mut frame: *const NuiImageFrame = std::ptr::null();
            let hr = next(stream, 0, &raw mut frame);
            if hr < 0 || frame.is_null() {
                println!("  stream {i}: event set but GetNextFrame -> 0x{:08X}", hr.cast_unsigned());
                continue;
            }
            counts[i] += 1;
            let f = &*frame;
            let tex = f.texture.cast::<c_void>();
            let vt = &**f.texture;
            let mut lr = LockedRect { pitch: 0, size: 0, bits: null_mut() };
            (vt.lock_rect)(tex, 0, &raw mut lr, null_mut(), 0);
            if counts[i] == 1 {
                println!(
                    "  {} frame #{} ts {} ms, pitch {} size {}",
                    if i == 0 { "color" } else { "depth" },
                    f.frame_number,
                    f.timestamp,
                    lr.pitch,
                    lr.size
                );
            }
            if !lr.bits.is_null() && lr.size > 0 {
                let data = std::slice::from_raw_parts(lr.bits, lr.size as usize);
                if i == 0 {
                    last_color = Some(data.to_vec());
                } else {
                    let d: Vec<u16> = data.as_chunks().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
                    players_now = d.iter().filter(|&&v| v & 7 != 0).count();
                    if players_now > best.as_ref().map_or_default(|b| b.0) {
                        best = Some((players_now, d.clone(), last_color.clone()));
                    }
                    last_depth = Some(d);
                }
            }
            (vt.unlock_rect)(tex, 0);
            release(stream, frame);
        }
        let secs = start.elapsed().as_secs_f64();
        println!("wait results (color, depth, skeleton, timeout): {waits:?}");
        println!(
            "color {} ({:.1} fps), depth {} ({:.1} fps), skeleton frames {} (max tracked {tracked_max},              {tracked_frames} frames with a tracked skeleton)",
            counts[0],
            counts[0] as f64 / secs,
            counts[1],
            counts[1] as f64 / secs,
            counts[2]
        );
        if let Some((n, d, c)) = &best {
            println!("  best frame: {n} player-labelled pixels -> nui_players.ppm");
            save_ppm_players("nui_players.ppm", 320, 240, d);
            if let Some(c) = c {
                save_ppm_bgra("nui_players_color.ppm", 640, 480, c);
            }
        }
        if let Some(c) = &last_color {
            save_ppm_bgra("nui_color.ppm", 640, 480, c);
        }
        if let Some(d) = &last_depth {
            let players = d.iter().filter(|&&v| v & 7 != 0).count();
            println!("  last depth frame: {players} player-labelled pixels");
            save_pgm(
                "nui_depth.pgm",
                320,
                240,
                d.iter().map(|&v| {
                    let mm = v >> 3;
                    if mm == 0 { 0 } else { (255 - (mm.min(4000) as u32 * 255 / 4000) as u8).max(1) }
                }),
            );
        }
        if no_shutdown {
            println!("exiting without NuiShutdown");
            return if counts[0] > 0 && counts[1] > 0 { 0 } else { 1 };
        }
        shutdown();
        println!("NuiShutdown done");
        if counts[0] > 0 && counts[1] > 0 { 0 } else { 1 }
    }
}
