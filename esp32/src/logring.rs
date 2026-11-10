//! A log ring buffer that stays through a restart, and the crash report of the last restart.
//!
//! The two rings are in RTC slow memory. A panic, a watchdog or a software restart does not clear
//! that memory. A power-on fills it with random data, and the magic number then does not match.
//! Each start writes into the other ring, so the ring of the previous start stays as it is, and the
//! heap keeps no copy of it. `GET /log` shows the lines of the previous start and the lines of this
//! start.

use std::ffi::CStr;
use std::fmt::Write as _;
use std::mem::MaybeUninit;
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use esp_idf_svc::log::EspIdfLogger;
use esp_idf_svc::sys;
use log::{LevelFilter, Log, Metadata, Record};

/// The size of each ring. The ESP32 has 8 KB of RTC slow memory, and other components use about
/// 60 bytes of it. The linker stops when the rings do not fit.
const SIZE: usize = 4000;
/// The firmware cuts a longer line, so that one line cannot fill the ring.
const MAX_LINE: usize = 512;
/// "XLG2". The rings are valid only when they start with this number. The single ring of the older
/// firmware used "XLOG", so the first start after an update does not read it as two rings.
const MAGIC: u32 = 0x584C_4732;

#[repr(C)]
struct Ring {
    /// The number of bytes written since the start. `total % SIZE` is the next write position.
    total: u32,
    buf: [u8; SIZE],
}

#[repr(C)]
struct Rings {
    magic: u32,
    /// The index of the ring of this start. The other ring has the lines of the previous start.
    current: u32,
    ring: [Ring; 2],
}

#[link_section = ".rtc_noinit"]
static mut RINGS: MaybeUninit<Rings> = MaybeUninit::uninit();
/// Each access to `RINGS` holds this lock.
static LOCK: Mutex<()> = Mutex::new(());
/// True when the other ring has the lines of the previous start. False after a power-on.
static PREVIOUS: AtomicBool = AtomicBool::new(false);
/// The crash report of this start. A flood of log lines cannot push it out of the ring.
static CRASH: Mutex<String> = Mutex::new(String::new());

static LOGGER: Logger = Logger;
static UART: EspIdfLogger = EspIdfLogger::new(());

struct Logger;

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        UART.log(record);
        let mut line = String::new();
        // SAFETY: esp_log_timestamp has no preconditions.
        let ms = unsafe { sys::esp_log_timestamp() };
        let _ = write!(
            line,
            "{ms} {} {}: {}",
            &record.level().as_str()[..1],
            record.target(),
            record.args()
        );
        if line.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        line.push('\n');
        push(line.as_bytes());
    }

    fn flush(&self) {}
}

/// Use the pointer only while you hold LOCK.
fn rings() -> *mut Rings {
    addr_of_mut!(RINGS).cast()
}

/// The ring of this start (`previous` false) or of the previous start (`previous` true). Use the
/// pointer only while you hold LOCK, and only after init().
fn ring(previous: bool) -> *mut Ring {
    // SAFETY: init() made `current` 0 or 1.
    unsafe {
        let r = rings();
        let i = (addr_of!((*r).current).read() as usize & 1) ^ usize::from(previous);
        addr_of_mut!((*r).ring[i])
    }
}

fn push(bytes: &[u8]) {
    // ponytail: a panic in this function deadlocks the panic hook on LOCK. The task watchdog
    // then restarts the box, and the core dump shows the cause. Use try_lock in the hook if
    // this occurs.
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: this function holds LOCK, and init() made the ring valid.
    let r = unsafe { &mut *ring(false) };
    for &b in bytes {
        r.buf[r.total as usize % SIZE] = b;
        r.total = r.total.wrapping_add(1);
    }
}

/// The text in the ring. After a wrap, the text starts at the first full line.
fn contents(r: &Ring) -> String {
    let total = r.total as usize;
    if total <= SIZE {
        return String::from_utf8_lossy(&r.buf[..total]).into_owned();
    }
    let start = total % SIZE;
    let mut bytes = r.buf[start..].to_vec();
    bytes.extend_from_slice(&r.buf[..start]);
    let first = bytes.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[first..]).into_owned()
}

/// Keeps the ring of the previous start, clears the other ring and installs the logger and the
/// panic hook. Call this function first in `main`.
pub fn init() {
    {
        let _guard = LOCK.lock().unwrap();
        let r = rings();
        // SAFETY: this function holds LOCK. The volatile reads accept the random data after a
        // power-on.
        unsafe {
            let valid = addr_of!((*r).magic).read_volatile() == MAGIC;
            let previous = addr_of!((*r).current).read_volatile() & 1;
            // The ring of the previous start becomes the other ring.
            addr_of_mut!((*r).current).write(if valid { previous ^ 1 } else { 0 });
            addr_of_mut!((*r).magic).write(MAGIC);
            (*ring(false)).total = 0;
            PREVIOUS.store(valid, Ordering::Relaxed);
        }
    }
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(LevelFilter::Info);
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}");
        default_hook(info);
    }));
}

/// The text for `GET /log`: the lines of the previous start, then the lines of this start.
pub fn text() -> String {
    let (current, previous) = {
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: this function holds LOCK. init() made the ring of this start valid, and
        // PREVIOUS shows that the other ring is valid.
        unsafe {
            let previous = if PREVIOUS.load(Ordering::Relaxed) {
                contents(&*ring(true))
            } else {
                String::new()
            };
            (contents(&*ring(false)), previous)
        }
    };
    let previous = if previous.is_empty() {
        "(no lines: the box started from power-on)\n"
    } else {
        previous.as_str()
    };
    let crash = CRASH.lock().unwrap_or_else(|e| e.into_inner());
    let crash = if crash.is_empty() {
        "(no core dump)\n"
    } else {
        crash.as_str()
    };
    format!("=== The last crash ===\n{crash}=== The previous start ===\n{previous}=== This start ===\n{current}")
}

/// Logs the core dump of the last crash, then erases it. The next crash can then write a new dump.
///
/// The core dump has the task, the exception and the backtrace. To get the source lines, run
/// `xtensa-esp32-elf-addr2line -pfiaC -e <the ELF of this build> <the addresses>`.
pub fn report_core_dump() {
    // SAFETY: the core dump calls read and erase only the coredump partition.
    unsafe {
        match sys::esp_core_dump_image_check() {
            sys::ESP_OK => {}
            sys::ESP_ERR_NOT_FOUND => return,
            e => {
                log::warn!("The core dump is not valid ({e}). The firmware erases it.");
                sys::esp_core_dump_image_erase();
                return;
            }
        }
        let mut reason = [0u8; 200];
        if sys::esp_core_dump_get_panic_reason(reason.as_mut_ptr().cast(), reason.len())
            == sys::ESP_OK
        {
            let reason = CStr::from_bytes_until_nul(&reason)
                .map(CStr::to_string_lossy)
                .unwrap_or_default();
            crash_line(format!("The last crash: {reason}"));
        }
        let mut s: sys::esp_core_dump_summary_t = std::mem::zeroed();
        if sys::esp_core_dump_get_summary(&mut s) == sys::ESP_OK {
            let task = CStr::from_ptr(s.exc_task.as_ptr()).to_string_lossy();
            let bt = &s.exc_bt_info;
            let depth = (bt.depth as usize).min(bt.bt.len());
            let addrs: Vec<String> = bt.bt[..depth]
                .iter()
                .map(|a| format!("{a:#010x}"))
                .collect();
            crash_line(format!(
                "The last crash: task {task}, PC {:#010x}, cause {}, address {:#010x}{}",
                s.exc_pc,
                s.ex_info.exc_cause,
                s.ex_info.exc_vaddr,
                if bt.corrupted {
                    ", the backtrace is corrupt"
                } else {
                    ""
                }
            ));
            crash_line(format!("The last crash: backtrace {}", addrs.join(" ")));
        }
        sys::esp_core_dump_image_erase();
    }
}

/// Logs a line of the crash report, and keeps it for the top of `GET /log`.
fn crash_line(line: String) {
    log::error!("{line}");
    let mut crash = CRASH.lock().unwrap_or_else(|e| e.into_inner());
    crash.push_str(&line);
    crash.push('\n');
}
