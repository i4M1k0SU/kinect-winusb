//! Camera command protocol over EP0 and the driver's command tables
//! (spec §3).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{NtStatus, status};
use crate::winusb::WinUsbDevice;
use log::{debug, error, trace};

pub const REPLY_LEN: usize = 0x200;
/// Offset of the reply data `D[]` (after the 8-byte header and the u16 status).
pub const DATA: usize = 10;

/// EP0 command channel (one command/reply exchange at a time).
pub struct CommandChannel {
    usb: Arc<WinUsbDevice>,
    tag: u16,
    timeout: Duration,
}

impl CommandChannel {
    pub fn new(usb: Arc<WinUsbDevice>) -> CommandChannel {
        CommandChannel { usb, tag: 0, timeout: Duration::from_millis(2000) }
    }

    /// Send a command and wait for its reply (spec §3.3). Like the driver, a reply
    /// is accepted only when its status word is 0; if none arrives in time the
    /// result is a zeroed reply with the status of the last poll.
    pub fn exchange(&mut self, opcode: u16, payload: &[u16]) -> Result<Box<[u8; REPLY_LEN]>, NtStatus> {
        let mut cmd = Vec::with_capacity(8 + payload.len() * 2);
        cmd.extend_from_slice(&0x4D47u16.to_le_bytes());
        cmd.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        cmd.extend_from_slice(&opcode.to_le_bytes());
        cmd.extend_from_slice(&self.tag.to_le_bytes());
        self.tag = self.tag.wrapping_add(1);
        for w in payload {
            cmd.extend_from_slice(&w.to_le_bytes());
        }
        trace!("cmd -> {}", crate::hex(&cmd));
        if let Err(e) = self.usb.control(0x40, 0, 0, 0, &mut cmd) {
            error!("camera command 0x{opcode:02x} send failed: {e}");
            return Err(status::from_error(&e));
        }
        let mut reply = Box::new([0u8; REPLY_LEN]);
        let start = Instant::now();
        let mut polls = 0u32;
        let mut last: Result<(), NtStatus>;
        loop {
            reply.fill(0);
            match self.usb.control(0xC0, 0, 0, 0, &mut reply[..]) {
                Ok(n) if n > 0 => {
                    last = Ok(());
                    let st = u16::from_le_bytes([reply[8], reply[9]]);
                    if st == 0 {
                        trace!("cmd 0x{opcode:02x} reply ({n} B, {polls} polls): {}", crate::hex(&reply[..n.min(64)]));
                        return Ok(reply);
                    }
                    debug!("cmd 0x{opcode:02x}: reply status 0x{st:04x}, polling on");
                }
                Ok(_) => last = Ok(()),
                Err(e) => last = Err(status::from_error(&e)),
            }
            polls += 1;
            if start.elapsed() > self.timeout {
                error!("camera command 0x{opcode:02x} {:?}: no reply after {polls} polls", payload);
                reply.fill(0);
                return last.map(|_| reply);
            }
            if polls > 8 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    pub fn write_register(&mut self, reg: u16, value: u16) -> Result<(), NtStatus> {
        self.exchange(0x03, &[reg, value]).map(|_| ())
    }
}

/// Values the driver caches from command replies (spec §2.6).
pub struct DeviceParams {
    pub version: [u8; 18],
    pub const_shift: u16,
    pub registration: [u8; 116],
    pub fixed_params: [u8; 168],
    pub ref_distance: i32,
    pub ref_pixel_size: f32,
    pub emitter_distance: f32,
    pub param24: f32,
    /// ctx+14784 / +14788 / +14792 / +14796.
    pub blanking: [i32; 4],
    pub depth_clipped: Box<[u16; 2048]>,
    pub depth_full: Box<[u16; 4096]>,
}

impl Default for DeviceParams {
    fn default() -> Self {
        DeviceParams {
            version: [0; 18],
            const_shift: 0,
            registration: [0; 116],
            fixed_params: [0; 168],
            ref_distance: 0,
            ref_pixel_size: 0.0,
            emitter_distance: 0.0,
            param24: 0.0,
            blanking: [0; 4],
            depth_clipped: Box::new([0; 2048]),
            depth_full: Box::new([0; 4096]),
        }
    }
}

fn f32_at(d: &[u8], i: usize) -> f32 {
    f32::from_le_bytes(*d[4 * i..4 * i + 4].as_array().unwrap())
}

impl DeviceParams {
    /// `sub_10EB0`: store the reply of a table command.
    /// Whether [`Self::process_reply`] keeps the reply of `table_op`.
    fn keeps_reply(table_op: u8) -> bool {
        matches!(table_op, 0x02 | 0x04 | 0x20 | 0x22 | 0x4B)
    }

    pub fn process_reply(&mut self, table_op: u8, reply: &[u8; REPLY_LEN]) {
        let d = &reply[DATA..];
        match table_op {
            0x02 => {
                let v = &mut self.version;
                v[0] = d[1];
                v[1] = d[0];
                v[2..10].copy_from_slice(&d[2..10]);
                v[12..18].copy_from_slice(&d[12..18]);
            }
            0x04 => self.const_shift = u16::from_le_bytes([d[0], d[1]]),
            0x20 => self.registration.copy_from_slice(&d[..116]),
            0x22 => {
                self.fixed_params.copy_from_slice(&d[..168]);
                self.ref_distance = f32_at(d, 25) as i32;
                self.ref_pixel_size = f32_at(d, 26);
                self.emitter_distance = f32_at(d, 23);
                self.param24 = f32_at(d, 24);
            }
            0x4B => {
                self.blanking[0] = ((f32_at(d, 0) as f64 * 1000.0) as f32) as i32;
                self.blanking[1] = f32_at(d, 1) as i32;
                self.blanking[2] = ((f32_at(d, 2) as f64 * 1000.0) as f32) as i32;
                self.blanking[3] = f32_at(d, 3) as i32;
            }
            _ => {}
        }
    }

    fn depth_value(&self, i: i32) -> f64 {
        let pix = self.ref_pixel_size as f64;
        let emit = self.emitter_distance as f64;
        let r = self.ref_distance as f64;
        let k = i - 4 * self.const_shift as i32;
        let x = k as f64 * 0.25 * pix;
        (x * r / (emit - k as f64 * 0.25 * pix) + r) * 10.0
    }

    /// `sub_17C44` (800..4000 mm) / `sub_17D5C` (400..3000 mm, near mode).
    pub fn build_clipped(&mut self, near: bool) {
        let (lo, hi) = if near { (400.0, 3000.0) } else { (800.0, 4000.0) };
        self.depth_clipped.fill(0);
        for i in 1..2048 {
            let d = self.depth_value(i);
            if d > 0.0 && d > lo && d < hi {
                self.depth_clipped[i as usize] = (d as i32) as u16;
            }
        }
    }

    /// `sub_17B44`.
    pub fn build_full(&mut self) {
        self.depth_full.fill(0);
        for i in 1..2048 {
            let d = self.depth_value(i);
            if d > 0.0 {
                self.depth_full[i as usize] = (d as i32) as u16;
            }
        }
    }
}

/// What a command-table opcode does (spec §3.5).
enum TableOp {
    End { rebuild_tables: bool },
    Command(u16, Vec<u16>),
}

fn decode(op: u8) -> Option<TableOp> {
    use TableOp::*;
    let reg = |r: u16, v: u16| Some(Command(0x03, vec![r, v]));
    match op {
        0x00 => Some(End { rebuild_tables: true }),
        0x3C | 0x3D | 0x48 => Some(End { rebuild_tables: false }),
        0x02 => Some(Command(0x00, vec![])),
        0x04 => Some(Command(0x16, vec![0, 0, 1, 30, 0])),
        0x06 => Some(Command(0x01, vec![])),
        0x07 => Some(Command(0x06, vec![3])),
        0x09 => reg(0x05, 1),
        0x0A => reg(0x05, 0),
        0x0B => reg(0x0C, 5),
        0x0C => reg(0x0D, 1),
        0x0D => reg(0x0E, 30),
        0x0F => reg(0x06, 2),
        0x10 => reg(0x06, 0),
        0x11 => reg(0x13, 1),
        0x12 => reg(0x14, 30),
        0x13 => reg(0x15, 7),
        0x15 => reg(0x17, 0),
        0x18 => reg(0x05, 3),
        0x1E => reg(0x12, 3),
        0x20 => Some(Command(0x16, vec![0x40, 0, 1, 30, 0])),
        0x22 => Some(Command(0x04, vec![0])),
        0x32 => reg(0x0C, 0),
        0x33 => reg(0x0C, 1),
        0x3B => reg(0x105, 0),
        0x3E => reg(0x0E, 15),
        0x3F => reg(0x0D, 2),
        0x40 => reg(0x1A, 2),
        0x41 => reg(0x1B, 15),
        0x42 => reg(0x19, 1),
        0x43 => reg(0x47, 0),
        0x44 => reg(0x2EF, 0),
        0x45 => reg(0x2EF, 400),
        0x46 => reg(0x15, 30),
        0x47 => reg(0x0E, 24),
        0x49 => Some(Command(0x86, vec![0])),
        0x4A => Some(Command(0x86, vec![1])),
        0x4B => Some(Command(0x16, vec![6, 0, 1, 30, 0])),
        _ => None,
    }
}

pub mod tables {
    pub const INIT: &[u8] = &[0x02, 0x06, 0x07, 0x06, 0x04, 0x20, 0x4B, 0x22, 0x00];
    pub const IR: &[u8] = &[0x0A, 0x33, 0x0C, 0x0D, 0x18, 0x3D];
    pub const HIRES_IR: &[u8] = &[0x0A, 0x42, 0x40, 0x41, 0x18, 0x3D];
    pub const BAYER: &[u8] = &[0x0A, 0x32, 0x0C, 0x0D, 0x09, 0x3D];
    pub const HIRES_BAYER: &[u8] = &[0x0A, 0x32, 0x3F, 0x3E, 0x09, 0x3D];
    pub const YUV: &[u8] = &[0x0A, 0x0B, 0x0C, 0x3E, 0x09, 0x3D];
    pub const COLOR_OFF: &[u8] = &[0x0A, 0x3D];
    pub const NEAR_ON: &[u8] = &[0x13, 0x44, 0x3C];
    pub const NEAR_OFF: &[u8] = &[0x46, 0x45, 0x3C];
    pub const DEPTH_ON: &[u8] = &[0x10, 0x1E, 0x11, 0x12, 0x3B, 0x0F, 0x3C];
    pub const DEPTH_OFF: &[u8] = &[0x10, 0x3C];
    pub const LASER_ON: &[u8] = &[0x4A, 0x48];
    pub const LASER_OFF: &[u8] = &[0x49, 0x48];

    /// CMOS register tables (cmd 0x96 {1, addr, value}), spec §3.6.
    pub const CMOS_BAYER: &[(u16, u16)] = &[
        (0x3C00, 0x5202),
        (0xC834, 0x0001),
        (0xC984, 0x8038),
        (0xFF01, 0x0028),
        (0xC808, 0x02DC),
        (0xC80A, 0x6C00),
        (0xC980, 0x0120),
        (0xFF01, 0x0028),
        (0xC800, 0x0004),
        (0xC802, 0x0004),
        (0xC804, 0x03CB),
        (0xC806, 0x050B),
        (0xC812, 0x03F0),
        (0xC814, 0x0634),
        (0xC818, 0x03C3),
        (0xFF01, 0x0028),
        (0xC858, 0x0500),
        (0xC85A, 0x03C0),
        (0xFF01, 0x0028),
    ];
    pub const CMOS_YUV: &[(u16, u16)] = &[
        (0xC800, 0x0004),
        (0xC802, 0x0000),
        (0xC804, 0x03CB),
        (0xC806, 0x050B),
        (0xC818, 0x03C3),
        (0xC834, 0x0001),
        (0xC854, 0x0000),
        (0xC856, 0x0000),
        (0xC858, 0x0500),
        (0xC85A, 0x03C0),
        (0xFF01, 0x0028),
        (0xC984, 0x8048),
        (0xFF01, 0x0028),
        (0xC80E, 0x01F4),
        (0xC810, 0x0258),
        (0xC812, 0x03F0),
        (0xC814, 0x0634),
        (0xFF01, 0x0028),
    ];
    pub const CMOS_DEFAULT: &[(u16, u16)] = &[
        (0x3C00, 0x5002),
        (0xC834, 0x0331),
        (0xC984, 0x8050),
        (0xFF01, 0x0028),
        (0xC808, 0x016E),
        (0xC80A, 0x3600),
        (0xC980, 0x0110),
        (0xFF01, 0x0028),
        (0xC800, 0x0000),
        (0xC802, 0x0000),
        (0xC804, 0x03CD),
        (0xC806, 0x050D),
        (0xC812, 0x02A4),
        (0xC814, 0x049E),
        (0xC818, 0x01E3),
        (0xFF01, 0x0028),
        (0xC858, 0x0280),
        (0xC85A, 0x01E0),
        (0xFF01, 0x0028),
    ];
}

/// `sub_123C8`: run a command table. Like the driver, command failures are
/// ignored (logged) and the result is always success.
pub fn run_table(ch: &mut CommandChannel, params: &std::sync::Mutex<DeviceParams>, near: bool, ops: &[u8]) {
    for &op in ops {
        match decode(op) {
            Some(TableOp::End { rebuild_tables }) => {
                if rebuild_tables {
                    let mut p = params.lock().unwrap();
                    let _ = near; // the init terminator always uses the default range
                    p.build_clipped(false);
                    p.build_full();
                }
                return;
            }
            Some(TableOp::Command(opcode, payload)) => match ch.exchange(opcode, &payload) {
                // Lock only for a reply that is kept: the stream-off and laser
                // tables also run during process exit (see `lock_unless_exiting`).
                Ok(reply) if DeviceParams::keeps_reply(op) => params.lock().unwrap().process_reply(op, &reply),
                Ok(_) => {}
                Err(st) => error!("table op 0x{op:02x} (cmd 0x{opcode:02x}) failed: 0x{:08X}", st.cast_unsigned()),
            },
            None => error!("unknown command table op 0x{op:02x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Calibration values read from a real Xbox 360 Kinect (045E:02AE).
    fn params() -> DeviceParams {
        DeviceParams {
            const_shift: 200,
            ref_distance: 120,
            ref_pixel_size: 0.1042,
            emitter_distance: 7.5,
            ..Default::default()
        }
    }

    #[test]
    fn depth_tables_match_the_driver_formula() {
        let mut p = params();
        p.build_full();
        p.build_clipped(false);
        // Values observed through IOCTL 0x22000C on the device.
        assert_eq!(p.depth_clipped[800], 1200);
        assert_eq!(p.depth_clipped[1000], 3930);
        assert_eq!(p.depth_clipped[600], 0, "below 800 mm is clipped");
        assert_eq!(p.depth_full[800], 1200);
        assert!(p.depth_full[600] > 0);
        assert_eq!(p.depth_full[0], 0);
        p.build_clipped(true);
        assert!(p.depth_clipped[1000] == 0, "near range ends at 3000 mm");
    }

    #[test]
    fn version_reply_is_byte_swapped_like_the_driver() {
        let mut r = Box::new([0u8; REPLY_LEN]);
        for i in 0..18 {
            r[DATA + i] = i as u8 + 1;
        }
        let mut p = DeviceParams::default();
        p.process_reply(0x02, &r);
        assert_eq!(&p.version[..], &[2, 1, 3, 4, 5, 6, 7, 8, 9, 10, 0, 0, 13, 14, 15, 16, 17, 18]);
    }
}
