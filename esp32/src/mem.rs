//! The memory report of `GET /mem`: the heap, the free stack of each task and the DMX output pin.
//!
//! Use the report to set the stack sizes. The free stack of a task is its low point since the
//! start. A task with much free stack has a stack that is too large.

use std::ffi::CStr;
use std::fmt::Write as _;

use esp_idf_svc::sys;

/// Builds the report as plain text.
pub fn report() -> String {
    let mut out = String::with_capacity(2048);
    for (name, caps) in [
        ("8-bit heap (DRAM)", sys::MALLOC_CAP_8BIT),
        ("32-bit heap (DRAM and IRAM)", sys::MALLOC_CAP_32BIT),
    ] {
        let mut i = sys::multi_heap_info_t::default();
        // SAFETY: `i` is a valid output buffer.
        unsafe { sys::heap_caps_get_info(&mut i, caps) };
        let _ = writeln!(
            out,
            "{name}: {} free, {} minimum, {} largest block, {} allocated in {} blocks, {} free blocks",
            i.total_free_bytes,
            i.minimum_free_bytes,
            i.largest_free_block,
            i.total_allocated_bytes,
            i.allocated_blocks,
            i.free_blocks,
        );
    }

    // SAFETY: uxTaskGetNumberOfTasks has no preconditions.
    let count = unsafe { sys::uxTaskGetNumberOfTasks() } as usize;
    // Tasks can start between the two calls, so leave space for some more.
    let mut tasks = vec![sys::TaskStatus_t::default(); count + 4];
    // SAFETY: the array has `tasks.len()` entries. The total run time can be null.
    let n = unsafe {
        sys::uxTaskGetSystemState(tasks.as_mut_ptr(), tasks.len() as u32, std::ptr::null_mut())
    };
    tasks.truncate(n as usize);
    tasks.sort_by_key(|t| std::cmp::Reverse(t.usStackHighWaterMark));
    let _ = writeln!(
        out,
        "\nFree stack of each task (low point since the start, in bytes):"
    );
    for t in &tasks {
        // SAFETY: FreeRTOS keeps the name of a live task in its control block.
        let name = unsafe { CStr::from_ptr(t.pcTaskName) }.to_string_lossy();
        let _ = writeln!(out, "{:>6}  {name}", t.usStackHighWaterMark);
    }
    #[cfg(feature = "dmx")]
    dmx_pin(&mut out);
    out
}

/// Writes the configuration of GPIO4, the DMX output. Signal 17 of the GPIO matrix is UART1 TX.
#[cfg(feature = "dmx")]
fn dmx_pin(out: &mut String) {
    const PIN: sys::gpio_num_t = 4;
    let mut buf = [0u8; 768];
    // SAFETY: the stream writes only into `buf`. fclose ends the stream before the code reads
    // `buf`.
    unsafe {
        let f = sys::fmemopen(buf.as_mut_ptr().cast(), buf.len(), c"w".as_ptr());
        if !f.is_null() {
            sys::gpio_dump_io_configuration(f, 1 << PIN);
            sys::fclose(f);
        }
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let _ = write!(
        out,
        "\nDMX output pin:\n{}",
        String::from_utf8_lossy(&buf[..end])
    );
}

/// The number of edges that one capture of the DMX output pin keeps. Each edge takes 4 bytes.
#[cfg(feature = "dmx")]
const MAX_EDGES: usize = 4000;

/// The time of one capture of the DMX output pin, in ms. It holds about 2.5 frames.
#[cfg(feature = "dmx")]
pub const SCOPE_MS: u32 = 60;

/// One capture of GPIO4 (DMX OUT).
#[cfg(feature = "dmx")]
pub struct Capture {
    /// The CPU clock in MHz. The times are in CPU cycles.
    pub mhz: u32,
    /// The level of the pin at the start.
    pub first: u32,
    /// Each entry is the time after the start in cycles. Bit 0 is the new level. The loop can
    /// stop for more than 2 µs. Two entries with bit 1 set then give the start and the end of
    /// the stop. An edge in the stop has no exact time.
    pub edges: Vec<u32>,
}

/// Reads the level of the DMX output pin for `SCOPE_MS` and records each edge.
///
/// A task with priority 24 on core 0 reads the pad in a loop. Interrupts can stop the loop for
/// some µs, and the capture marks each such stop. The loop blocks the WiFi task for the capture
/// time. The `dmx` and `midi` threads on core 1 keep running.
#[cfg(feature = "dmx")]
pub fn scope() -> std::io::Result<Capture> {
    use esp_idf_svc::hal::cpu::Core;
    crate::spawn_rt(c"scope", 4096, 24, Some(Core::Core0), capture)?
        .join()
        .map_err(|_| std::io::Error::other("the capture panicked"))
}

#[cfg(feature = "dmx")]
fn capture() -> Capture {
    const PIN: u32 = 4;
    let mhz = sys::CONFIG_ESP_DEFAULT_CPU_FREQ_MHZ;
    let span = SCOPE_MS * 1000 * mhz;
    let gap = 2 * mhz;
    // SAFETY: GPIO_IN_REG is a read-only register of the GPIO peripheral. The UART drives the
    // pad, and its input buffer is on (`gpio_dump_io_configuration` shows InputEn: 1).
    let read = || (unsafe { std::ptr::read_volatile(sys::GPIO_IN_REG as *const u32) } >> PIN) & 1;
    // SAFETY: xthal_get_ccount reads the cycle counter of the current core.
    let ccount = || unsafe { sys::xthal_get_ccount() };
    let mut edges = Vec::with_capacity(MAX_EDGES);
    let start = ccount();
    let first = read();
    let mut level = first;
    let mut last = start;
    loop {
        let now = ccount();
        let l = read();
        let t = now.wrapping_sub(start);
        if t >= span || edges.len() >= MAX_EDGES {
            break;
        }
        let stopped = now.wrapping_sub(last) > gap;
        if stopped {
            // The stop starts at the last read. This entry keeps the old level.
            edges.push((last.wrapping_sub(start) & !3) | 2 | level);
        }
        if l != level || stopped {
            edges.push((t & !3) | u32::from(stopped) << 1 | l);
            level = l;
        }
        last = now;
    }
    Capture { mhz, first, edges }
}
