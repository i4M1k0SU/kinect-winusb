//! Isochronous stream reception and frame assembly (spec §5).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Devices::Usb::USBD_ISO_PACKET_DESCRIPTOR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::core::Owned;

use super::{ReadRequest, lock_unless_exiting, status};
use crate::winusb::WinUsbDevice;
use log::{debug, error, info};

/// USBD_STATUS_DATA_UNDERRUN — accepted like success by the driver.
const USBD_DATA_UNDERRUN: i32 = 0xC000_0009_u32.cast_signed();
pub const HEADER_LEN: usize = 32;

/// Packet error statistics shared by both streams (IOCTL 0x220010, 13 u32).
pub type PacketStats = Mutex<[u32; 13]>;

/// QPC time in microseconds (`sub_17EE8`).
pub fn now_us() -> i64 {
    let mut c = 0i64;
    let mut f = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&raw mut c);
        let _ = QueryPerformanceFrequency(&raw mut f);
    }
    if f == 0 {
        return 0;
    }
    1_000_000 * (c / f) + 1_000_000 * (c % f) / f
}

struct Filling {
    req: ReadRequest,
    /// Payload bytes copied so far (the request's Information, header excluded).
    info: usize,
}

pub struct StreamState {
    /// Manual "pending" queue started (false after a stop until the next start).
    pub accepting: bool,
    pending: VecDeque<ReadRequest>,
    current: Option<Filling>,
    pub frame_no: u32,
    t_start: i64,
    t_end: i64,
    pub expected: usize,
    /// 28-byte statistics block (7 u32), IOCTL 0x220014 / 0x220018.
    pub stats: [u32; 7],
    pub zero_length: u32,
    pub pending_count: u32,
}

/// One isochronous stream: 0 = color/IR (EP 0x81), 1 = depth (EP 0x82).
pub struct Stream {
    pub index: usize,
    pub endpoint: u8,
    pub bytes_per_interval: u32,
    pub state: Mutex<StreamState>,
    reader: Mutex<Option<Reader>>,
    pub in_flight: AtomicU32,
    pub transfers_done: AtomicU32,
}

struct Reader {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Stream {
    pub fn new(index: usize, endpoint: u8, bytes_per_interval: u32, expected: usize) -> Stream {
        Stream {
            index,
            endpoint,
            bytes_per_interval,
            state: Mutex::new(StreamState {
                accepting: true,
                pending: VecDeque::new(),
                current: None,
                frame_no: 0,
                t_start: 0,
                t_end: 0,
                expected,
                stats: [0; 7],
                zero_length: 0,
                pending_count: 0,
            }),
            reader: Mutex::new(None),
            in_flight: AtomicU32::new(0),
            transfers_done: AtomicU32::new(0),
        }
    }

    fn now(&self) -> i64 {
        if self.index == 1 { now_us() - 3600 } else { now_us() }
    }

    /// Queue a validated read (forward to the manual queue).
    pub fn queue_read(&self, req: ReadRequest) {
        let mut st = self.state.lock().unwrap();
        st.pending_count = st.pending_count.wrapping_add(1);
        if !st.accepting {
            drop(st);
            req.complete(status::WDF_BUSY, 0);
            return;
        }
        st.pending.push_back(req);
    }

    /// Cancel the queued (not yet filling) reads of one file, plus the filling one.
    pub fn cancel_file(&self, file_id: u64) {
        let mut cancelled = Vec::new();
        {
            let Some(mut st) = lock_unless_exiting(&self.state) else {
                return;
            };
            let mut keep = VecDeque::new();
            while let Some(r) = st.pending.pop_front() {
                if r.file_id == file_id { cancelled.push(r) } else { keep.push_back(r) }
            }
            st.pending = keep;
            // Unlike the kernel (where the in-progress read is not cancelable), the
            // caller's buffer must not be written after its handle is gone.
            if let Some(c) = st.current.take_if(|c| c.req.file_id == file_id) {
                cancelled.push(c.req);
            }
        }
        for r in cancelled {
            r.complete(status::CANCELLED, 0);
        }
    }

    /// `sub_121C8`: abort the read being filled.
    fn abort_current(st: &mut StreamState) {
        if let Some(c) = st.current.take() {
            st.stats[2] += 1;
            st.pending_count = st.pending_count.wrapping_sub(1);
            c.req.complete(status::REQUEST_ABORTED, 0);
        }
    }

    /// Process one isochronous packet (`sub_13CF4`).
    fn parse_packet(&self, st: &mut StreamState, pkt: &[u8], pstats: &mut [u32; 13]) {
        let s = self.index;
        if pkt.len() < 12 {
            pstats[2] += 1;
            return;
        }
        if u16::from_le_bytes([pkt[0], pkt[1]]) != 0x4252 {
            pstats[1] += 1;
            return;
        }
        if u16::from_be_bytes([pkt[6], pkt[7]]) as usize != pkt.len() {
            pstats[3] += 1;
            return;
        }
        let kind = match u16::from_le_bytes([pkt[2], pkt[3]]) {
            0x7100 | 0x8100 => 0,
            0x7200 | 0x8200 => 1,
            0x7500 | 0x8500 => 2,
            0xDEAD => {
                pstats[10] += 1;
                return;
            }
            0xF31F => {
                pstats[11] += 1;
                return;
            }
            0xFADE => {
                pstats[12] += 1;
                return;
            }
            _ => {
                pstats[4] += 1;
                return;
            }
        };
        // Counter quirk: SOF bumps all three, MID two, EOF one.
        for c in &mut st.stats[4 + kind..7] {
            *c = c.wrapping_add(1);
        }
        let payload = &pkt[12..];
        match kind {
            0 => {
                if let Some(cur) = st.current.as_mut() {
                    pstats[5 + s] += 1;
                    pstats[8 + s] += 1;
                    cur.info = 0;
                } else {
                    st.frame_no = st.frame_no.wrapping_add(1);
                    st.t_start = self.now();
                    match st.pending.pop_front() {
                        Some(req) => st.current = Some(Filling { req, info: 0 }),
                        None => st.stats[3] += 1,
                    }
                }
                Self::append(st, payload);
            }
            1 => Self::append(st, payload),
            _ => {
                st.t_end = self.now();
                if st.current.is_some() {
                    Self::append(st, payload);
                }
                if let Some(mut cur) = st.current.take() {
                    let mut hdr = [0u8; HEADER_LEN];
                    hdr[0..4].copy_from_slice(&st.frame_no.to_le_bytes());
                    hdr[8..16].copy_from_slice(&st.t_start.to_le_bytes());
                    hdr[16..24].copy_from_slice(&st.t_end.to_le_bytes());
                    unsafe { std::ptr::copy_nonoverlapping(hdr.as_ptr(), cur.req.buf.ptr, HEADER_LEN) };
                    cur.info += HEADER_LEN;
                    if cur.info == st.expected {
                        st.stats[1] += 1;
                        st.pending_count = st.pending_count.wrapping_sub(1);
                        let n = st.expected;
                        cur.req.complete(status::SUCCESS, n);
                    } else {
                        pstats[8 + s] += 1;
                        cur.info = 0;
                        st.current = Some(cur);
                    }
                }
            }
        }
    }

    /// `sub_138E4`: append payload behind the header area.
    fn append(st: &mut StreamState, payload: &[u8]) {
        let Some(cur) = st.current.as_mut() else {
            return;
        };
        let off = HEADER_LEN + cur.info;
        if off + payload.len() > cur.req.buf.len {
            Self::abort_current(st);
            return;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), cur.req.buf.ptr.add(off), payload.len());
        }
        cur.info += payload.len();
    }

    /// `start_data_path` (`sub_14F20`).
    pub fn start(self: &Arc<Self>, usb: &Arc<WinUsbDevice>, pstats: &Arc<PacketStats>, expected: usize) {
        {
            let mut st = self.state.lock().unwrap();
            st.stats = [0; 7];
            st.expected = expected;
            st.accepting = true;
        }
        *pstats.lock().unwrap() = [0; 13];
        let mut reader = self.reader.lock().unwrap();
        if reader.is_some() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (s, u, p, st) = (self.clone(), usb.clone(), pstats.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name(format!("kinect-iso-{:02x}", self.endpoint))
            .spawn(move || s.run(&u, &p, &st))
            .expect("spawn isoch reader");
        *reader = Some(Reader { stop, thread });
    }

    /// `stop_data_path` (`sub_14E40`).
    pub fn stop(&self, usb: &WinUsbDevice) {
        let Some(mut st) = lock_unless_exiting(&self.state) else {
            return;
        };
        st.accepting = false;
        let waiting: Vec<ReadRequest> = st.pending.drain(..).collect();
        let current = st.current.take();
        if current.is_some() {
            st.stats[2] += 1;
            st.pending_count = st.pending_count.wrapping_sub(1);
        }
        drop(st);
        for r in waiting {
            r.complete(status::CANCELLED, 0);
        }
        if let Some(c) = current {
            c.req.complete(status::REQUEST_ABORTED, 0);
        }
        self.stop_reader(usb);
    }

    pub fn stop_reader(&self, usb: &WinUsbDevice) {
        // During process exit the reader thread is already gone: joining it would panic.
        if crate::process_exiting() {
            return;
        }
        let reader = self.reader.lock().unwrap().take();
        if let Some(r) = reader {
            // No WinUsb_AbortPipe here: after aborting an isochronous pipe,
            // WinUSB fails every later ASAP transfer on it with ERROR_BUSY.
            // The reader stops requeueing and lets in-flight transfers finish
            // (at most XFERS * PACKETS microframes, ~100 ms).
            r.stop.store(true, Ordering::Release);
            let _ = usb;
            let _ = r.thread.join();
        }
    }

    /// Reader thread: keep `XFERS` isochronous transfers queued and feed every
    /// received packet to the parser, in order.
    fn run(&self, usb: &WinUsbDevice, pstats: &PacketStats, stop: &AtomicBool) {
        const PACKETS: u32 = 32;
        const XFERS: usize = 24;

        struct Slot {
            ov: Box<OVERLAPPED>,
            event: Owned<HANDLE>,
            descs: Vec<USBD_ISO_PACKET_DESCRIPTOR>,
            queued: bool,
        }

        let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
        let per = self.bytes_per_interval;
        let xfer_len = per * PACKETS;
        let mut buffer = vec![0u8; xfer_len as usize * XFERS];
        // Only raw pointers into the buffer are used: the device writes into the
        // regions of queued transfers concurrently.
        let buf_ptr = buffer.as_mut_ptr();
        if let Err(e) = usb.reset_pipe(self.endpoint) {
            debug!("reset pipe 0x{:02x}: {e}", self.endpoint);
        }
        let iso = match unsafe { usb.register_isoch_buffer(self.endpoint, buf_ptr, buffer.len()) } {
            Ok(b) => b,
            Err(e) => {
                error!("WinUsb_RegisterIsochBuffer(ep 0x{:02x}) failed: {e}", self.endpoint);
                return;
            }
        };
        let mut slots = Vec::from_fn(XFERS, |_| Slot {
            ov: Box::default(),
            event: unsafe { Owned::new(CreateEventW(None, true, false, None).expect("event")) },
            descs: vec![USBD_ISO_PACKET_DESCRIPTOR::default(); PACKETS as usize],
            queued: false,
        });
        let submit = |slot: &mut Slot, i: usize, cont: bool| -> bool {
            *slot.ov = OVERLAPPED { hEvent: *slot.event, ..Default::default() };
            let r = unsafe { iso.read_asap(i as u32 * xfer_len, xfer_len, cont, &mut slot.descs, &raw const *slot.ov) };
            match r {
                Ok(()) => {
                    slot.queued = true;
                    true
                }
                Err(e) => {
                    debug!("ReadIsochPipeAsap(ep 0x{:02x}, continue={cont}) failed: {e}", self.endpoint);
                    false
                }
            }
        };
        let drain = |slots: &mut Vec<Slot>| {
            for s in slots.iter_mut() {
                if s.queued {
                    let _ = unsafe { usb.overlapped_result(&raw const *s.ov, true) };
                    s.queued = false;
                }
            }
        };

        info!("isoch reader ep 0x{:02x} started ({} x {} packets of {} B)", self.endpoint, XFERS, PACKETS, per);
        let mut errors = 0u32;
        'outer: while !stop.load(Ordering::Acquire) {
            // (Re)start the stream: queue every slot, the first without ContinueStream.
            let mut ok = true;
            for (i, slot) in slots.iter_mut().enumerate() {
                if !(submit(slot, i, i != 0) || submit(slot, i, false)) {
                    ok = false;
                    break;
                }
            }
            if !ok {
                drain(&mut slots);
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            self.in_flight.store(XFERS as u32, Ordering::Relaxed);
            let mut i = 0usize;
            loop {
                let r = unsafe { usb.overlapped_result(&raw const *slots[i].ov, true) };
                slots[i].queued = false;
                if stop.load(Ordering::Acquire) {
                    break 'outer;
                }
                match r {
                    Ok(_) => {
                        errors = 0;
                        self.transfers_done.fetch_add(1, Ordering::Relaxed);
                        let mut st = self.state.lock().unwrap();
                        let mut ps = pstats.lock().unwrap();
                        let base = i * xfer_len as usize;
                        for d in &slots[i].descs {
                            if d.Status >= 0 || d.Status == USBD_DATA_UNDERRUN {
                                if d.Length == 0 {
                                    st.zero_length = st.zero_length.wrapping_add(1);
                                } else {
                                    let off = (d.Offset as usize).min(xfer_len as usize);
                                    let len = (d.Length as usize).min(xfer_len as usize - off);
                                    let pkt = unsafe { std::slice::from_raw_parts(buf_ptr.add(base + off), len) };
                                    self.parse_packet(&mut st, pkt, &mut ps);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if errors <= 3 || errors.is_multiple_of(100) {
                            error!("isoch transfer on ep 0x{:02x} failed: {e} (#{errors})", self.endpoint);
                        }
                        Self::abort_current(&mut self.state.lock().unwrap());
                        drain(&mut slots);
                        if errors > 20 {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        continue 'outer;
                    }
                }
                if !(submit(&mut slots[i], i, true) || submit(&mut slots[i], i, false)) {
                    error!("cannot requeue isoch transfer on ep 0x{:02x}", self.endpoint);
                    drain(&mut slots);
                    continue 'outer;
                }
                i = (i + 1) % XFERS;
            }
        }
        // Wait for everything still queued before the buffer goes away.
        drain(&mut slots);
        self.in_flight.store(0, Ordering::Relaxed);
        drop(iso);
        drop(slots);
        drop(buffer);
        let st = self.snapshot_stats();
        let ps = *pstats.lock().unwrap();
        info!(
            "isoch reader ep 0x{:02x} stopped: SOF {}, frames done {}, aborted {}, no reader {}, bad size {}; packets bad {:?}",
            self.endpoint,
            st[4],
            st[1],
            st[2],
            st[3],
            ps[8 + self.index],
            &ps[1..5]
        );
    }

    pub fn snapshot_stats(&self) -> [u32; 7] {
        self.state.lock().unwrap().stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::{NtStatus, UserBuffer};
    use std::sync::mpsc;

    const SOF: u8 = 0x71;
    const MID: u8 = 0x72;
    const EOF: u8 = 0x75;

    fn pkt(flag: u8, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![b'R', b'B', 0x00, flag, 0, 0];
        p.extend_from_slice(&((12 + payload.len()) as u16).to_be_bytes());
        p.extend_from_slice(&[0; 4]);
        p.extend_from_slice(payload);
        p
    }

    struct Harness {
        stream: Stream,
        pstats: [u32; 13],
        rx: mpsc::Receiver<(NtStatus, usize, Vec<u8>)>,
        tx: mpsc::Sender<(NtStatus, usize, Vec<u8>)>,
    }

    impl Harness {
        fn new(expected: usize) -> Harness {
            let (tx, rx) = mpsc::channel();
            Harness { stream: Stream::new(1, 0x82, 1920, expected), pstats: [0; 13], rx, tx }
        }
        fn read(&self, len: usize) {
            let mut buf = vec![0xAAu8; len];
            let ub = UserBuffer { ptr: buf.as_mut_ptr(), len };
            let tx = self.tx.clone();
            self.stream.queue_read(ReadRequest::new(
                ub,
                Box::new(move |st, n| {
                    let _ = tx.send((st, n, buf));
                }),
            ));
        }
        fn feed(&mut self, p: &[u8]) {
            let mut st = self.stream.state.lock().unwrap();
            self.stream.parse_packet(&mut st, p, &mut self.pstats);
        }
    }

    #[test]
    fn assembles_a_frame_with_header() {
        let mut h = Harness::new(32 + 9);
        h.read(64);
        h.feed(&pkt(SOF, &[1, 2, 3]));
        h.feed(&pkt(MID, &[4, 5, 6]));
        h.feed(&pkt(EOF, &[7, 8, 9]));
        let (st, n, buf) = h.rx.try_recv().expect("read completed");
        assert_eq!(st, status::SUCCESS);
        assert_eq!(n, 41);
        assert_eq!(u32::from_le_bytes(*buf[0..4].as_array().unwrap()), 1, "frame number");
        assert_eq!(&buf[4..8], &[0; 4]);
        let t0 = i64::from_le_bytes(*buf[8..16].as_array().unwrap());
        let t1 = i64::from_le_bytes(*buf[16..24].as_array().unwrap());
        assert!(t1 >= t0);
        assert_eq!(&buf[24..32], &[0; 8]);
        assert_eq!(&buf[32..41], &[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let s = h.stream.snapshot_stats();
        assert_eq!(s[1], 1, "completed ok");
        assert_eq!((s[4], s[5], s[6]), (1, 2, 3), "packet counters quirk");
    }

    #[test]
    fn short_frame_keeps_the_read_for_the_next_frame() {
        let mut h = Harness::new(32 + 6);
        h.read(64);
        h.feed(&pkt(SOF, &[1, 2]));
        h.feed(&pkt(EOF, &[3])); // 3 bytes != 6: dropped
        assert!(h.rx.try_recv().is_err());
        assert_eq!(h.pstats[9], 1, "depth frame discarded");
        h.feed(&pkt(SOF, &[1, 2, 3]));
        h.feed(&pkt(EOF, &[4, 5, 6]));
        let (st, n, buf) = h.rx.try_recv().expect("second frame completes the same read");
        assert_eq!((st, n), (status::SUCCESS, 38));
        assert_eq!(&buf[32..38], &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn frames_without_a_reader_are_dropped_but_counted() {
        let mut h = Harness::new(32 + 2);
        h.feed(&pkt(SOF, &[1]));
        h.feed(&pkt(EOF, &[2]));
        h.read(64);
        h.feed(&pkt(SOF, &[3]));
        h.feed(&pkt(EOF, &[4]));
        let (_, _, buf) = h.rx.try_recv().unwrap();
        assert_eq!(u32::from_le_bytes(*buf[0..4].as_array().unwrap()), 2, "frame numbers keep counting");
        assert_eq!(h.stream.snapshot_stats()[3], 1, "SOF without reader");
    }

    #[test]
    fn overflow_aborts_the_read() {
        let mut h = Harness::new(32 + 100);
        h.read(32 + 4);
        h.feed(&pkt(SOF, &[0; 8]));
        let (st, n, _) = h.rx.try_recv().unwrap();
        assert_eq!((st, n), (status::REQUEST_ABORTED, 0));
    }

    #[test]
    fn rejects_malformed_packets() {
        let mut h = Harness::new(40);
        h.feed(&[b'R', b'B', 0, SOF]); // short
        let mut bad = pkt(SOF, &[1]);
        bad[0] = b'X';
        h.feed(&bad);
        let mut size = pkt(SOF, &[1]);
        size[7] += 1;
        h.feed(&size);
        h.feed(&pkt(0x99, &[1]));
        let mut dead = pkt(0, &[1]);
        dead[2] = 0xAD;
        dead[3] = 0xDE;
        h.feed(&dead);
        assert_eq!(&h.pstats[1..5], &[1, 1, 1, 1]);
        assert_eq!(h.pstats[10], 1);
    }

    #[test]
    fn stop_cancels_waiting_reads_and_rejects_new_ones() {
        let h = Harness::new(40);
        h.read(64);
        {
            let mut st = h.stream.state.lock().unwrap();
            st.accepting = false;
            let w: Vec<ReadRequest> = st.pending.drain(..).collect();
            drop(st);
            for r in w {
                r.complete(status::CANCELLED, 0);
            }
        }
        assert_eq!(h.rx.try_recv().unwrap().0, status::CANCELLED);
        h.read(64);
        assert_eq!(h.rx.try_recv().unwrap().0, status::WDF_BUSY);
    }
}
