//! The memory report of `GET /mem`: the heap and the free stack of each task.
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
    out
}
