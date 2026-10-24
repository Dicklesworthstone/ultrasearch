use std::sync::atomic::{AtomicU64, Ordering};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

static WORKING_SET_TRIMS: AtomicU64 = AtomicU64::new(0);

/// Return the number of times working-set / heap trimming has been triggered.
pub fn working_set_trim_count() -> u64 {
    WORKING_SET_TRIMS.load(Ordering::Relaxed)
}

/// Trim unused allocator/process working-set pages when transitioning into DeepIdle.
pub fn trim_working_set() {
    WORKING_SET_TRIMS.fetch_add(1, Ordering::Relaxed);

    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::Memory::{HEAP_FLAGS, GetProcessHeap, HeapCompact};
        unsafe {
            if let Ok(heap) = GetProcessHeap() {
                let _ = HeapCompact(heap, HEAP_FLAGS(0));
            }
        }
    }
}
