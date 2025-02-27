use utralib::generated::*;
use core::num::{NonZeroU8, NonZeroU16};
use std::cell::RefCell;

pub const BUFLEN: usize = 1024 * 512;

#[repr(C)]
pub struct PerfLogEntry {
    code: u32,
    ts: u32,
}

/// Pure client for performance monitoring. It is not capable of emptying the FIFO or
/// detecting an overflow, but it can register an event through a somewhat-abstracted interface.
pub struct PerfClient {
    event_csr: AtomicCsr<u32>,
}
impl PerfClient {
    pub fn new(
        event_csr: AtomicCsr<u32>,
    ) -> Self {
        Self {
            event_csr,
        }
    }
    #[inline(always)]
    pub fn log_event_unchecked(&self, code: u32) {
        // event_sourceN is identical across each block, so we can take advantage of the flexibility of
        // UTRA to do a non-specific cross-crate register access pattern.
        self.event_csr.wfo(utra::event_source1::PERFEVENT_CODE,
            code);
    }
}

/// Manager interface for performance monitoring. This also includes a performance event port.
pub struct PerfMgr<'a> {
    data: RefCell::<&'a mut [PerfLogEntry]>,
    perf_csr: AtomicCsr<u32>,
    event_csr: AtomicCsr<u32>,
    log_index: RefCell<usize>,
    ts_cont: RefCell<u64>,
    // these are expressed as raw values to be written to hardware
    saturation_limit: u64,
    prescaler: u16,
    saturate: bool,
    event_bit_width: u8,
    // debug fields
    dbg_buf_count: RefCell<u32>,
}

impl <'a> PerfMgr<'a> {
    pub fn new(
        log_ptr: *mut u8,
        perf_csr: AtomicCsr<u32>,
        event_csr: AtomicCsr<u32>,
    ) -> Self {
        let data =
            unsafe{
                core::slice::from_raw_parts_mut(
                    log_ptr as *mut PerfLogEntry,
                    BUFLEN / core::mem::size_of::<PerfLogEntry>()
                )
            };
        // initialize the buffer region when the manager is created
        for d in data.iter_mut() {
            d.code = 0;
            d.ts = 0;
        }
        Self {
            data: RefCell::new(data),
            perf_csr,
            event_csr,
            log_index: RefCell::new(0),
            ts_cont: RefCell::new(0),
            saturation_limit: 0xffff_ffff,
            prescaler: 0, // 1 clock per sample
            event_bit_width: 31, // 32 bits width
            saturate: true,
            dbg_buf_count: RefCell::new(0),
        }
    }

    /// Sets the saturation limit for the performance counter. Units in clock cycles.
    /// If `None`, the counter will freely rollover without stopping the performance counting process.
    #[allow(dead_code)]
    pub fn sat_limit(&mut self, limit: Option<u64>) {
        if let Some(l) = limit {
            self.saturation_limit = l;
            self.saturate = true;
        } else {
            self.saturate = false;
            self.saturation_limit = u64::MAX;
        }
    }

    /// Zero is an invalid value for clocks per sample. Hence NonZeroU16 type.
    #[allow(dead_code)]
    pub fn clocks_per_sample(&mut self, cps: NonZeroU16) {
        self.prescaler = cps.get() - 1;
    }

    /// Sets the width of the event code. This is enforced by hardware, software is free to pass in a code that is too large.
    /// Excess bits will be ignored, starting from the MSB side. A bitwdith of 0 is illegal. A bitwidth larger than 32 is set to 32.
    #[allow(dead_code)]
    pub fn code_bitwidth(&mut self, bitwidth: NonZeroU8) {
        let bw = bitwidth.get();
        if bw > 32 {
            self.event_bit_width = 31;
        } else {
            self.event_bit_width = bw - 1;
        }
    }

    pub fn stop_and_reset(&self) {
        for d in self.data.borrow_mut().iter_mut() {
            d.code = 0;
            d.ts = 0;
        }
        self.perf_csr.wfo(utra::perfcounter::RUN_STOP, 1);
        while self.perf_csr.rf(utra::perfcounter::STATUS_READABLE) == 1 {
            let i = self.perf_csr.r(utra::perfcounter::EVENT_INDEX); // this advances the FIFO until it is empty
            log::warn!("FIFO was not drained before reset: {}", i);
        }
        // stop the counter if it would rollover
        self.perf_csr.wo(utra::perfcounter::SATURATE_LIMIT0, self.saturation_limit as u32);
        self.perf_csr.wo(utra::perfcounter::SATURATE_LIMIT1, (self.saturation_limit >> 32) as u32);

        // configure the system
        self.perf_csr.wo(utra::perfcounter::CONFIG,
            self.perf_csr.ms(utra::perfcounter::CONFIG_PRESCALER, self.prescaler as u32)
            | self.perf_csr.ms(utra::perfcounter::CONFIG_SATURATE, if self.saturate {1} else {0})
            | self.perf_csr.ms(utra::perfcounter::CONFIG_EVENT_WIDTH_MINUS_ONE, self.event_bit_width as u32)
        );
        self.log_index.replace(0);
        self.ts_cont.replace(0);
    }

    pub fn start(&self) {
        self.perf_csr.wfo(utra::perfcounter::RUN_RESET_RUN, 1);
    }

    /// This function is convenient, but the overhead of the checking adds a lot of cache line noise to the data
    /// If you are looking to get very cycle-accurate counts, use `log_event_unchecked` with manual flush calls
    ///
    /// returns Ok(()) if the event could be logged
    /// returns an Err if the performance buffer would overflow
    #[inline(always)]
    pub fn log_event(&self, code: u32) -> Result::<(), xous::Error> {
        self.flush_if_full()?;
        self.event_csr.wfo(utra::event_source1::PERFEVENT_CODE,
                code);
        Ok(())
    }

    #[allow(dead_code)]
    #[inline(always)]
    pub fn log_event_unchecked(&self, code: u32) {
        self.event_csr.wfo(utra::event_source1::PERFEVENT_CODE,
            code);
    }
    #[allow(dead_code)]
    pub fn flush(&self) -> Result::<(), xous::Error> {
        let mut oom = false;
        // stop the counter
        self.perf_csr.wfo(utra::perfcounter::RUN_STOP, 1);
        // copy over the events to the long term buffer
        let mut ts_offset = 0;
        let mut expected_i = 0;
        while self.perf_csr.rf(utra::perfcounter::STATUS_READABLE) == 1 {
            if *self.log_index.borrow() < self.data.borrow().len() {
                self.dbg_buf_count.replace(self.dbg_buf_count.take() + 1); // this tracks total entries copied into the perfbuf
                (*self.data.borrow_mut())[*self.log_index.borrow()].code = self.perf_csr.r(utra::perfcounter::EVENT_RAW0);
                ts_offset = self.perf_csr.r(utra::perfcounter::EVENT_RAW1) as u32;
                (*self.data.borrow_mut())[*self.log_index.borrow()].ts = *self.ts_cont.borrow() as u32 + ts_offset;

                let i = self.perf_csr.r(utra::perfcounter::EVENT_INDEX); // this advances the FIFO
                if i != expected_i & 0xfff {
                    log::info!("i {} != expected_i {}", i, expected_i);
                }
                expected_i += 1;
                self.log_index.replace(self.log_index.take() + 1);
            } else {
                oom = true;
                break;
            }
        }
        // update the timestamp continuation field with the last timestamp seen; the next line
        // resets the timestamp counter to 0 again.
        self.ts_cont.replace(self.ts_cont.take() + ts_offset as u64);
        // restart the counter
        if !oom {
            // duplicate this code down here because we want the reset and log to be as close as possible to the return statement
            self.perf_csr.wfo(utra::perfcounter::RUN_RESET_RUN, 1);
            Ok(())
        } else {
            self.perf_csr.wfo(utra::perfcounter::RUN_RESET_RUN, 1);
            Err(xous::Error::OutOfMemory)
        }
    }

    #[allow(dead_code)]
    pub fn flush_if_full(&self) -> Result::<(), xous::Error> {
        // check to see if the FIFO is full first. If so, drain it.
        if self.perf_csr.rf(utra::perfcounter::STATUS_FULL) != 0 {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Flushes any data in the FIFO to the performance buffer. Also stops the performance counter from running.
    /// Returns the total number of entries in the buffer, or an OOM if the buffer is full
    /// This will update the time stamp rollover counter, so, you could in theory restart after this without losing state.
    pub fn stop_and_flush(&self) -> Result::<u32, xous::Error> {
        let mut oom = false;
        // stop the counter
        self.perf_csr.wfo(utra::perfcounter::RUN_STOP, 1);
        // copy over the events to the long term buffer
        let mut ts_offset = 0;
        let mut expected_i = 0;
        while self.perf_csr.rf(utra::perfcounter::STATUS_READABLE) == 1 {
            if *self.log_index.borrow() < self.data.borrow().len() {
                self.dbg_buf_count.replace(self.dbg_buf_count.take() + 1); // this tracks total entries copied into the perfbuf
                self.data.borrow_mut()[*self.log_index.borrow()].code = self.perf_csr.r(utra::perfcounter::EVENT_RAW0);
                ts_offset = self.perf_csr.r(utra::perfcounter::EVENT_RAW1) as u32;
                self.data.borrow_mut()[*self.log_index.borrow()].ts = *self.ts_cont.borrow() as u32 + ts_offset;

                let i = self.perf_csr.r(utra::perfcounter::EVENT_INDEX); // this advances the FIFO
                if i != expected_i {
                    log::info!("i {} != expected_i {}", i, expected_i);
                }
                expected_i += 1;
                self.log_index.replace(self.log_index.take() + 1);
            } else {
                oom = true;
                break;
            }
        }
        self.ts_cont.replace(self.ts_cont.take() + ts_offset as u64);
        log::info!("FIFO final flush had {} entries", expected_i - 1);
        log::info!("Had {} buffer entries", *self.dbg_buf_count.borrow()); // should be the same as above, but trying to chase down some minor issues...
        if !oom {
            Ok(*self.dbg_buf_count.borrow())
        } else {
            Err(xous::Error::OutOfMemory)
        }
    }

    #[allow(dead_code)]
    pub fn print_page_table(&self) {
        log::info!("Buf vmem loc: {:x}", self.data.borrow().as_ptr() as u32);
        match xous::syscall::virt_to_phys(self.data.borrow().as_ptr() as usize) {
            Ok(addr) => log::info!("got {}", addr),
            Err(e) => log::info!("error: {:?}", e),
        };
        log::info!("Buf pmem loc: {:x}", xous::syscall::virt_to_phys(self.data.borrow().as_ptr() as usize).unwrap_or(0));
        log::info!("PerfLogEntry size: {}", core::mem::size_of::<PerfLogEntry>());
        log::info!("Now printing the page table mapping for the performance buffer:");
        for page in (0..BUFLEN).step_by(4096) {
            log::info!("V|P {:x} {:x}",
                self.data.borrow().as_ptr() as usize + page,
                xous::syscall::virt_to_phys(self.data.borrow().as_ptr() as usize + page).unwrap_or(0),
            );
        }
    }
}
