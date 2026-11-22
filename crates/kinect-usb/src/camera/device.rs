//! The emulated camera device: USB setup (spec §2) and IOCTL dispatch (spec §4).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows::Win32::Devices::Usb::{PIPE_TRANSFER_TIMEOUT, UsbdPipeTypeIsochronous};

use super::ioctl::*;
use super::protocol::{CommandChannel, DATA, DeviceParams, run_table, tables};
use super::stream::{PacketStats, Stream};
use super::{NtStatus, ReadRequest, lock_unless_exiting, status};
use crate::winusb::{PipeInfo, WinUsbDevice};
use log::{debug, error, info};

const DEPTH_MIN_READ: usize = 0x96020;
const DEPTH_FRAME: usize = 422_432;

pub struct Camera {
    usb: Arc<WinUsbDevice>,
    pub path: String,
    config_desc: Vec<u8>,
    alt_settings: u8,
    pipes: Vec<PipeInfo>,
    cmd: Mutex<CommandChannel>,
    /// The driver's sequential command queue.
    queue: Mutex<()>,
    params: Mutex<DeviceParams>,
    /// 0 = color/IR (EP 0x81), 1 = depth (EP 0x82).
    streams: [Arc<Stream>; 2],
    pstats: Arc<PacketStats>,
    color_mode: AtomicU32,
    hires_variant: AtomicU32,
    near_mode: AtomicU32,
    cmos_applied: AtomicU32,
    depth_streaming: AtomicU32,
    color_streaming: AtomicU32,
}

fn u32s_to_bytes(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Copy `data` to the output buffer of a "direct" IOCTL.
fn reply(output: &mut [u8], data: &[u8]) -> (NtStatus, usize) {
    if output.len() < data.len() {
        return (status::BUFFER_TOO_SMALL, data.len());
    }
    output[..data.len()].copy_from_slice(data);
    (status::SUCCESS, data.len())
}

fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(*b[off..off + 2].as_array().unwrap())
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(*b[off..off + 4].as_array().unwrap())
}

impl Camera {
    pub fn open(path: &str) -> Result<Camera, NtStatus> {
        info!("opening camera {path} over WinUSB");
        let usb = Arc::new(WinUsbDevice::open(path).map_err(|e| {
            error!("cannot open {path} with WinUSB: {e}");
            status::from_error(&e)
        })?);

        // Served by IOCTL GET_CONFIG_DESCRIPTOR; asking for more than wTotalLength
        // returns the whole configuration.
        let mut config_desc = vec![0u8; 1024];
        let n = usb.get_descriptor(2, 0, &mut config_desc).map_err(|e| status::from_error(&e))?;
        config_desc.truncate(n);
        // The Kinect for Windows camera (02BF) has a second alternate setting that
        // carries the isochronous endpoints; the Xbox 360 camera has only alt 0.
        let alt_settings: u8 = if usb.query_interface(1).is_ok() { 2 } else { 1 };
        let alt = alt_settings - 1;
        if alt == 1 {
            usb.set_alternate(1).map_err(|e| status::from_error(&e))?;
        }
        let ifd = usb.query_interface(alt).map_err(|e| status::from_error(&e))?;
        let mut pipes = Vec::new();
        for i in 0..ifd.bNumEndpoints {
            pipes.push(usb.query_pipe(alt, i).map_err(|e| status::from_error(&e))?);
        }
        info!("camera: {} alt setting(s), using alt {alt}, pipes {:?}", alt_settings, pipes);
        let bpi = |ep: u8| pipes.iter().find(|p| p.pipe_id == ep).map(|p| p.max_bytes_per_interval).unwrap_or(1920);
        let _ = usb.set_pipe_policy(0, PIPE_TRANSFER_TIMEOUT, 1000);
        for p in &pipes {
            let _ = usb.reset_pipe(p.pipe_id);
        }

        let cam = Camera {
            cmd: Mutex::new(CommandChannel::new(usb.clone())),
            streams: [
                Arc::new(Stream::new(0, 0x81, bpi(0x81), 307_232)),
                Arc::new(Stream::new(1, 0x82, bpi(0x82), DEPTH_FRAME)),
            ],
            usb,
            path: path.to_string(),
            config_desc,
            alt_settings,
            pipes,
            queue: Mutex::new(()),
            params: Mutex::new(DeviceParams::default()),
            pstats: Arc::new(Mutex::new([0; 13])),
            color_mode: AtomicU32::new(0),
            hires_variant: AtomicU32::new(0),
            near_mode: AtomicU32::new(0),
            cmos_applied: AtomicU32::new(0),
            depth_streaming: AtomicU32::new(0),
            color_streaming: AtomicU32::new(0),
        };
        {
            let mut ch = cam.cmd.lock().unwrap();
            // Not done by the kernel driver (which sees a freshly powered device):
            // make sure no stream is left running by a previous process.
            let _ = ch.write_register(0x05, 0);
            let _ = ch.write_register(0x06, 0);
            run_table(&mut ch, &cam.params, false, tables::INIT);
        }
        {
            let p = cam.params.lock().unwrap();
            info!(
                "camera init: firmware {:02x?}, const shift {}, ref distance {}, pixel size {}, emitter {}",
                p.version, p.const_shift, p.ref_distance, p.ref_pixel_size, p.emitter_distance
            );
        }
        Ok(cam)
    }

    pub fn pipe_count(&self) -> usize {
        self.pipes.len()
    }

    pub fn usb(&self) -> &Arc<WinUsbDevice> {
        &self.usb
    }

    fn run(&self, ops: &[u8]) {
        let near = self.near_mode.load(Ordering::Relaxed) != 0;
        match lock_unless_exiting(&self.cmd) {
            Some(mut ch) => run_table(&mut ch, &self.params, near, ops),
            // Process exit, and a terminated thread holds the channel: use a new one.
            None => run_table(&mut CommandChannel::new(self.usb.clone()), &self.params, near, ops),
        }
    }

    fn exchange(&self, opcode: u16, payload: &[u16]) -> Result<Box<[u8; 0x200]>, NtStatus> {
        self.cmd.lock().unwrap().exchange(opcode, payload)
    }

    fn cmos_table(&self, t: &[(u16, u16)]) -> NtStatus {
        for &(a, v) in t {
            if let Err(st) = self.exchange(0x96, &[1, a, v]) {
                return st;
            }
        }
        status::SUCCESS
    }

    /// `sub_1357C`: undo a CMOS table on devices with two alt settings.
    fn cmos_restore(&self) -> NtStatus {
        if self.alt_settings != 2 || self.cmos_applied.load(Ordering::Relaxed) == 0 {
            return status::SUCCESS;
        }
        self.run(tables::BAYER);
        let st = self.cmos_table(tables::CMOS_DEFAULT);
        if st < 0 {
            return st;
        }
        self.run(tables::COLOR_OFF);
        self.cmos_applied.store(0, Ordering::Relaxed);
        st
    }

    /// Expected size of a completed color/IR read for `mode` (`sub_13A50`).
    fn color_expected(&self, mode: u32) -> usize {
        match mode {
            0 => 307_232,
            2 => 390_432,
            3 => 614_432,
            4 => match self.hires_variant.load(Ordering::Relaxed) {
                1 => 1_310_752,
                2 | 3 => 1_228_832,
                _ => self.streams[0].state.lock().unwrap().expected,
            },
            5 => 1_638_432,
            _ => self.streams[0].state.lock().unwrap().expected,
        }
    }

    fn color_min_read(mode: u32) -> Option<usize> {
        match mode {
            0 => Some(0x4B020),
            2 => Some(0x5F520),
            3 => Some(0x96020),
            4 => Some(0x140020),
            5 => Some(0x190020),
            _ => None,
        }
    }

    fn start_color(&self, mode: u32, table: &[u8]) -> NtStatus {
        let st = self.cmos_restore();
        if st < 0 {
            return st;
        }
        self.color_mode.store(mode, Ordering::Relaxed);
        self.streams[0].state.lock().unwrap().pending_count = 0;
        self.run(table);
        let expected = self.color_expected(mode);
        self.streams[0].start(&self.usb, &self.pstats, expected);
        self.color_streaming.store(1, Ordering::Relaxed);
        info!("color stream started (mode {mode}, frame {expected} B)");
        status::SUCCESS
    }

    fn stop_color(&self) -> NtStatus {
        self.run(tables::COLOR_OFF);
        self.streams[0].stop(&self.usb);
        self.color_streaming.store(0, Ordering::Relaxed);
        info!("color stream stopped");
        status::SUCCESS
    }

    fn start_depth(&self) -> NtStatus {
        self.streams[1].state.lock().unwrap().pending_count = 0;
        self.run(tables::DEPTH_ON);
        self.streams[1].start(&self.usb, &self.pstats, DEPTH_FRAME);
        self.depth_streaming.store(1, Ordering::Relaxed);
        info!("depth stream started");
        status::SUCCESS
    }

    fn stop_depth(&self) -> NtStatus {
        self.run(tables::DEPTH_OFF);
        self.streams[1].stop(&self.usb);
        self.depth_streaming.store(0, Ordering::Relaxed);
        info!("depth stream stopped");
        status::SUCCESS
    }

    fn runtime_state(&self) -> Vec<u8> {
        let c = self.streams[0].state.lock().unwrap();
        let (c_zero, c_pend) = (c.zero_length, c.pending_count);
        drop(c);
        let d = self.streams[1].state.lock().unwrap();
        let (d_zero, d_pend) = (d.zero_length, d.pending_count);
        drop(d);
        u32s_to_bytes(&[
            self.near_mode.load(Ordering::Relaxed),
            self.cmos_applied.load(Ordering::Relaxed),
            self.streams[1].in_flight.load(Ordering::Relaxed),
            self.streams[0].in_flight.load(Ordering::Relaxed),
            self.streams[0].transfers_done.load(Ordering::Relaxed),
            self.streams[1].transfers_done.load(Ordering::Relaxed),
            self.depth_streaming.load(Ordering::Relaxed),
            self.color_streaming.load(Ordering::Relaxed),
            c_zero,
            d_zero,
            c_pend,
            d_pend,
        ])
    }

    /// Register access IOCTLs 0x22009C..0x2200A8.
    fn register_ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> (NtStatus, usize) {
        let read = code == CMOS_READ || code == CMOS_INDIRECT_READ;
        if input.len() < 4 || (read && output.len() < 4) {
            return (status::BUFFER_TOO_SMALL, 0);
        }
        if self.color_streaming.load(Ordering::Relaxed) == 0 {
            return (status::INVALID_DEVICE_REQUEST, 0);
        }
        let addr = read_u16(input, 0);
        let value = read_u16(input, 2);
        if read {
            output[0..2].copy_from_slice(&addr.to_le_bytes());
        }
        if !(addr >= 0xFF00 || (0xA000..=0xCFFF).contains(&addr)) {
            return (status::INVALID_PARAMETER, 0);
        }
        let payload: Vec<u16> = match code {
            CMOS_WRITE => vec![1, addr, value],
            CMOS_INDIRECT_WRITE => vec![2, 0xFF80, addr, 0xFF81, value],
            CMOS_READ => vec![1, 0xFF82, addr],
            _ => vec![1, 0xFF83, addr],
        };
        match self.exchange(0x96, &payload) {
            Ok(r) => {
                if !read {
                    return (status::SUCCESS, 0);
                }
                if r[DATA] & 1 == 0 {
                    return (status::UNSUCCESSFUL, 0);
                }
                output[2..4].copy_from_slice(&r[14..16]);
                (status::SUCCESS, 4)
            }
            Err(st) => (st, 0),
        }
    }

    fn nudge(&self, input: &[u8]) -> NtStatus {
        if input.len() < 8 {
            return status::BUFFER_TOO_SMALL;
        }
        let t = read_u32(input, 0);
        if read_u32(input, 4) != 0 {
            return status::INVALID_PARAMETER;
        }
        let (b2, b3) = {
            let p = self.params.lock().unwrap();
            (p.blanking[2].cast_unsigned(), p.blanking[3].cast_unsigned())
        };
        if b2 == 0 {
            error!("NudgeStreamTiming without blanking info");
            return status::INVALID_DEVICE_STATE;
        }
        let r = t.wrapping_sub(b3).wrapping_add(33333) % 33333;
        let v = ((1000u64 * r as u64) / b2 as u64).clamp(25, 2047) as u16;
        match self.exchange(0x22, &[v, 1, 1]) {
            Ok(_) => status::SUCCESS,
            Err(st) => st,
        }
    }

    fn raw_command(&self, input: &[u8]) -> NtStatus {
        const PS: usize = size_of::<usize>();
        let (ptr_off, n_off) = if PS == 8 { (8, 16) } else { (4, 8) };
        if input.len() < n_off + 4 {
            return status::INVALID_PARAMETER;
        }
        let opcode = read_u16(input, 0);
        let addr = usize::from_le_bytes(*input[ptr_off..ptr_off + PS].as_array().unwrap());
        let ptr = std::ptr::with_exposed_provenance::<u16>(addr);
        let n = read_u32(input, n_off) as usize;
        // The driver's `2 * n > 0x1F8`, without overflowing a 32-bit usize.
        if n > 0x1F8 / 2 || (ptr.is_null() && n > 1) {
            return status::INVALID_PARAMETER;
        }
        let payload = Vec::from_fn(n >> 1, |i| unsafe { ptr.add(i).read_unaligned() });
        match self.exchange(opcode, &payload) {
            Ok(_) => status::SUCCESS,
            Err(st) => st,
        }
    }

    /// DeviceIoControl. Returns (status, Information).
    pub fn ioctl(&self, pipe: Option<u8>, code: u32, input: &[u8], output: &mut [u8]) -> (NtStatus, usize) {
        let r = self.ioctl_inner(pipe, code, input, output);
        debug!(
            "IOCTL 0x{code:06X} (in {} out {}) -> 0x{:08X} info {}",
            input.len(),
            output.len(),
            r.0.cast_unsigned(),
            r.1
        );
        r
    }

    fn ioctl_inner(&self, pipe: Option<u8>, code: u32, input: &[u8], output: &mut [u8]) -> (NtStatus, usize) {
        // Direct IOCTLs (default queue).
        match code {
            GET_CONFIG_DESCRIPTOR => return reply(output, &self.config_desc),
            RESET_PIPE => {
                let Some(p) = pipe else {
                    return (status::INVALID_PARAMETER, 0);
                };
                let ep = self.pipes[p as usize].pipe_id;
                return match self.usb.reset_pipe(ep) {
                    Ok(()) => (status::SUCCESS, 0),
                    Err(e) => (status::from_error(&e), 0),
                };
            }
            GET_DEPTH_TABLE_CLIPPED => {
                let p = self.params.lock().unwrap();
                let b: Vec<u8> = p.depth_clipped.iter().flat_map(|v| v.to_le_bytes()).collect();
                return reply(output, &b);
            }
            GET_DEPTH_TABLE_FULL => {
                let p = self.params.lock().unwrap();
                let b: Vec<u8> = p.depth_full.iter().flat_map(|v| v.to_le_bytes()).collect();
                return reply(output, &b);
            }
            GET_PACKET_STATS => {
                return reply(output, &u32s_to_bytes(&*self.pstats.lock().unwrap()));
            }
            GET_COLOR_STATS => {
                return reply(output, &u32s_to_bytes(&self.streams[0].snapshot_stats()));
            }
            GET_DEPTH_STATS => {
                return reply(output, &u32s_to_bytes(&self.streams[1].snapshot_stats()));
            }
            NOP_34 => return (status::SUCCESS, 0),
            GET_FIRMWARE_VERSION => return reply(output, &self.params.lock().unwrap().version),
            RAW_COMMAND => return (self.raw_command(input), 0),
            GET_REGISTRATION => return reply(output, &self.params.lock().unwrap().registration),
            GET_FIXED_PARAMS => return reply(output, &self.params.lock().unwrap().fixed_params),
            GET_CONST_SHIFT => {
                return reply(output, &self.params.lock().unwrap().const_shift.to_le_bytes());
            }
            CYCLE_PORT => {
                info!("cycle port requested (not supported over WinUSB; reporting success)");
                return (status::SUCCESS, 0);
            }
            GET_RUNTIME_STATE => return reply(output, &self.runtime_state()),
            SET_HIRES_VARIANT => {
                if input.len() < 4 {
                    return (status::BUFFER_TOO_SMALL, 0);
                }
                self.hires_variant.store(read_u32(input, 0), Ordering::Relaxed);
                return (status::SUCCESS, 0);
            }
            NOP_94 => {
                return if input.len() < 8 { (status::BUFFER_TOO_SMALL, 0) } else { (status::SUCCESS, 8) };
            }
            READ_TRACE_LOG => {
                if input.len() < 4 || output.len() < 0x400 {
                    return (status::BUFFER_TOO_SMALL, 0);
                }
                output[0..4].copy_from_slice(&0u32.to_le_bytes());
                return (status::SUCCESS, 4);
            }
            _ => {}
        }

        // Forwarded IOCTLs (sequential command queue).
        let _q = lock_unless_exiting(&self.queue);
        let st = match code {
            RESET_DEVICE => {
                self.streams[1].stop(&self.usb);
                self.streams[0].stop(&self.usb);
                self.run(tables::INIT);
                status::SUCCESS
            }
            START_DEPTH => self.start_depth(),
            STOP_DEPTH => self.stop_depth(),
            START_BAYER => self.start_color(0, tables::BAYER),
            START_IR => self.start_color(2, tables::IR),
            START_YUV => self.start_color(3, tables::YUV),
            START_HIRES_BAYER => self.start_color(4, tables::HIRES_BAYER),
            START_HIRES_IR => self.start_color(5, tables::HIRES_IR),
            STOP_BAYER | STOP_IR | STOP_YUV | STOP_HIRES_IR | STOP_HIRES_BAYER => self.stop_color(),
            NEAR_MODE_ON | NEAR_MODE_OFF => {
                let near = code == NEAR_MODE_ON;
                self.near_mode.store(near as u32, Ordering::Relaxed);
                self.params.lock().unwrap().build_clipped(near);
                self.run(if near { tables::NEAR_ON } else { tables::NEAR_OFF });
                status::SUCCESS
            }
            LASER_ON => {
                self.run(tables::LASER_ON);
                status::SUCCESS
            }
            LASER_OFF => {
                self.run(tables::LASER_OFF);
                status::SUCCESS
            }
            CMOS_BAYER_SETTINGS | CMOS_YUV_SETTINGS => {
                if self.alt_settings != 2 {
                    status::SUCCESS
                } else {
                    let t = if code == CMOS_BAYER_SETTINGS { tables::CMOS_BAYER } else { tables::CMOS_YUV };
                    let st = self.cmos_table(t);
                    if st >= 0 {
                        self.cmos_applied.store(1, Ordering::Relaxed);
                    }
                    st
                }
            }
            NUDGE_STREAM_TIMING => self.nudge(input),
            CMOS_WRITE | CMOS_READ | CMOS_INDIRECT_WRITE | CMOS_INDIRECT_READ => {
                return self.register_ioctl(code, input, output);
            }
            _ => status::INVALID_DEVICE_REQUEST,
        };
        (st, 0)
    }

    /// ReadFile on a pipe handle (`sub_3D34C`, `sub_14148`, read gates).
    pub fn read(&self, pipe: Option<u8>, req: ReadRequest) {
        let Some(p) = pipe else {
            req.complete(status::INVALID_PARAMETER, 0);
            return;
        };
        let info = self.pipes[p as usize];
        if info.pipe_type != UsbdPipeTypeIsochronous {
            req.complete(status::INVALID_DEVICE_REQUEST, 0);
            return;
        }
        if info.max_bytes_per_interval == 0 || req.len() < info.max_bytes_per_interval as usize {
            req.complete(status::INVALID_PARAMETER, 0);
            return;
        }
        match info.pipe_id {
            0x81 => {
                let Some(min) = Self::color_min_read(self.color_mode.load(Ordering::Relaxed)) else {
                    req.complete(status::INVALID_PARAMETER, 0);
                    return;
                };
                if req.len() < min {
                    req.complete(status::BUFFER_TOO_SMALL, min);
                    return;
                }
                self.streams[0].queue_read(req);
            }
            0x82 => {
                if req.len() < DEPTH_MIN_READ {
                    req.complete(status::BUFFER_TOO_SMALL, DEPTH_MIN_READ);
                    return;
                }
                self.streams[1].queue_read(req);
            }
            _ => req.complete(status::SUCCESS, 0),
        }
    }

    pub fn cancel_file(&self, file_id: u64) {
        for s in &self.streams {
            s.cancel_file(file_id);
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        info!("closing camera {}", self.path);
        for s in &self.streams {
            s.stop_reader(&self.usb);
        }
    }
}
