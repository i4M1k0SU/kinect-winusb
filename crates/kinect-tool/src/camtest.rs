//! `camtest`: exercise the user-mode camera driver directly (no Kinect10.dll).

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::devices::{GUID_KINECT_CAMERA, interface_paths};
use kinect_usb::camera::{self, ReadRequest, UserBuffer, ioctl, status};

struct Frame {
    stream: usize,
    status: i32,
    len: usize,
    buf: Vec<u8>,
}

fn submit(file: &camera::CameraFile, stream: usize, size: usize, tx: &mpsc::Sender<Frame>) {
    let mut buf = vec![0u8; size];
    let ub = UserBuffer { ptr: buf.as_mut_ptr(), len: buf.len() };
    let tx = tx.clone();
    // The Vec is moved into the completion, which keeps its heap buffer alive
    // until the request finishes.
    let req = ReadRequest::new(
        ub,
        Box::new(move |st, n| {
            let _ = tx.send(Frame { stream, status: st, len: n, buf });
        }),
    );
    file.read(req);
}

/// Unpack the 11-bit big-endian packed depth stream.
pub fn unpack11(raw: &[u8], out: &mut [u16]) {
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut it = raw.iter();
    for o in out.iter_mut() {
        while bits < 11 {
            acc = (acc << 8) | *it.next().unwrap_or(&0) as u32;
            bits += 8;
        }
        bits -= 11;
        *o = ((acc >> bits) & 0x7FF) as u16;
    }
}

fn save_pgm(path: &str, w: usize, h: usize, px: impl Iterator<Item = u8>) {
    let mut data = format!("P5\n{w} {h}\n255\n").into_bytes();
    data.extend(px.take(w * h));
    if std::fs::write(path, data).is_ok() {
        println!("  wrote {path}");
    }
}

pub fn run(args: &[String]) -> i32 {
    let mut frames_wanted = 60usize;
    let mut restarts = 0usize;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--restarts" => restarts = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            v => frames_wanted = v.parse().unwrap_or(frames_wanted),
        }
    }
    kinect_usb::logger::init(true);
    let Some(path) = interface_paths(&GUID_KINECT_CAMERA, None).into_iter().next() else {
        eprintln!("no camera interface {{00873FDF-...}} found; run `kinect-winusb install` first");
        return 1;
    };
    println!("camera interface: {path}");
    let ctl = match camera::open(&path, "") {
        Ok(f) => f,
        Err(st) => {
            eprintln!("open failed: 0x{:08X}", st.cast_unsigned());
            return 1;
        }
    };
    let mut out = vec![0u8; 0x2000];
    let (st, n) = ctl.ioctl(ioctl::GET_FIRMWARE_VERSION, &[], &mut out[..20]);
    println!("firmware version block (0x{:08X}): {}", st.cast_unsigned(), kinect_usb::hex(&out[..n]));
    let (_, n) = ctl.ioctl(ioctl::GET_CONST_SHIFT, &[], &mut out[..2]);
    println!("const shift: {}", u16::from_le_bytes([out[0], out[1]]) * (n == 2) as u16);
    let (_, n) = ctl.ioctl(ioctl::GET_DEPTH_TABLE_CLIPPED, &[], &mut out[..4096]);
    let table: Vec<u16> = out[..n].as_chunks().0.iter().map(|&c| u16::from_le_bytes(c)).collect();
    println!("depth table: [600]={} [800]={} [1000]={}", table[600], table[800], table[1000]);

    let open_pipe = |suffix: &str| {
        camera::open(&path, suffix)
            .inspect_err(|st| eprintln!("open of {suffix} failed: 0x{:08X}", st.cast_unsigned()))
            .ok()
    };
    let Some(color) = open_pipe("\\PIPE00") else {
        return 1;
    };
    let Some(depth) = open_pipe("\\PIPE01") else {
        return 1;
    };
    let (tx, rx) = mpsc::channel();
    for r in 0..restarts {
        let (st, _) = ctl.ioctl(ioctl::START_DEPTH, &[], &mut []);
        println!("start depth (warm-up {r}): 0x{:08X}", st.cast_unsigned());
        std::thread::sleep(Duration::from_millis(500));
        let (st, _) = ctl.ioctl(ioctl::STOP_DEPTH, &[], &mut []);
        println!("stop depth (warm-up {r}): 0x{:08X}", st.cast_unsigned());
    }
    let (st, _) = ctl.ioctl(ioctl::START_DEPTH, &[], &mut []);
    println!("start depth: 0x{:08X}", st.cast_unsigned());
    let (st, _) = ctl.ioctl(ioctl::START_BAYER, &[], &mut []);
    println!("start bayer: 0x{:08X}", st.cast_unsigned());
    for _ in 0..3 {
        submit(&depth, 1, 0x96020, &tx);
        submit(&color, 0, 0x4B020, &tx);
    }

    let start = Instant::now();
    let mut count = [0usize; 2];
    let mut last: [Option<Frame>; 2] = [None, None];
    while (count[0] < frames_wanted || count[1] < frames_wanted) && start.elapsed() < Duration::from_secs(15) {
        let Ok(f) = rx.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        if f.status != status::SUCCESS {
            println!("  stream {} read failed: 0x{:08X}", f.stream, f.status.cast_unsigned());
            continue;
        }
        let no = u32::from_le_bytes(*f.buf[0..4].as_array().unwrap());
        let t0 = i64::from_le_bytes(*f.buf[8..16].as_array().unwrap());
        let t1 = i64::from_le_bytes(*f.buf[16..24].as_array().unwrap());
        count[f.stream] += 1;
        if count[f.stream] <= 3 || count[f.stream].is_multiple_of(30) {
            println!(
                "  {} frame #{no}: {} bytes, sof->eof {} us",
                if f.stream == 0 { "color" } else { "depth" },
                f.len,
                t1 - t0
            );
        }
        let s = f.stream;
        submit(if s == 0 { &color } else { &depth }, s, if s == 0 { 0x4B020 } else { 0x96020 }, &tx);
        last[s] = Some(f);
    }
    let secs = start.elapsed().as_secs_f64();
    println!("received color {} / depth {} frames in {secs:.1}s", count[0], count[1]);

    let mut stats = [0u8; 52];
    ctl.ioctl(ioctl::GET_PACKET_STATS, &[], &mut stats);
    println!("packet stats: {:?}", stats.as_chunks().0.iter().map(|&c| u32::from_le_bytes(c)).collect::<Vec<_>>());
    for (code, name) in [(ioctl::GET_COLOR_STATS, "color"), (ioctl::GET_DEPTH_STATS, "depth")] {
        let mut s = [0u8; 28];
        ctl.ioctl(code, &[], &mut s);
        println!("{name} stats: {:?}", s.as_chunks().0.iter().map(|&c| u32::from_le_bytes(c)).collect::<Vec<_>>());
    }
    ctl.ioctl(ioctl::STOP_DEPTH, &[], &mut []);
    ctl.ioctl(ioctl::STOP_BAYER, &[], &mut []);
    // Drain cancelled reads.
    while let Ok(f) = rx.recv_timeout(Duration::from_millis(300)) {
        let _ = f;
    }

    if let Some(f) = &last[1] {
        let mut d = vec![0u16; 640 * 480];
        unpack11(&f.buf[32..f.len], &mut d);
        let valid = d.iter().filter(|&&v| v < 2047).count();
        println!("depth: {valid} of {} pixels valid", d.len());
        save_pgm("depth.pgm", 640, 480, d.iter().map(|&v| if v >= 2047 { 0 } else { 255 - (v >> 3) as u8 }));
    }
    if let Some(f) = &last[0] {
        save_pgm("color_bayer.pgm", 640, 480, f.buf[32..f.len].iter().copied());
    }
    drop((color, depth, ctl));
    if count[0] > 0 && count[1] > 0 { 0 } else { 1 }
}
