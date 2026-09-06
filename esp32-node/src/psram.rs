//! A `std::alloc::Allocator` backed by `heap_caps_malloc(..., MALLOC_CAP_SPIRAM)`,
//! so specific collections (the punch queue in `main.rs`) can be pinned to
//! PSRAM explicitly, rather than hoping ESP-IDF's general-heap size
//! threshold happens to route them there. Requires `CONFIG_SPIRAM` +
//! `CONFIG_SPIRAM_USE_CAPS_ALLOC` in sdkconfig.defaults (PSRAM initialized,
//! but deliberately *not* folded into the general malloc/global allocator —
//! this stays opt-in per collection instead of an heap-wide policy change).
//!
//! Needs `#![feature(allocator_api)]` at the crate root (main.rs) — the
//! `Allocator` trait is nightly-only. The esp-rs Xtensa toolchain is itself
//! a nightly build, so this is available; a future switch to a stable
//! compiler would need this rewritten around a different mechanism (a
//! custom collection type, or `GlobalAlloc` instead).

use std::alloc::{AllocError, Allocator, Layout};
use std::ffi::c_void;
use std::ptr::NonNull;

use esp_idf_svc::sys::{heap_caps_free, heap_caps_malloc, MALLOC_CAP_8BIT, MALLOC_CAP_SPIRAM};

#[derive(Debug, Clone, Copy, Default)]
pub struct PsramAllocator;

unsafe impl Allocator for PsramAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            // Matches the standard allocators' contract for zero-size
            // layouts: a dangling, well-aligned, non-null pointer, no actual
            // allocation performed.
            return Ok(NonNull::slice_from_raw_parts(layout.dangling_ptr(), 0));
        }
        // heap_caps_malloc's own alignment guarantee is a plain size_t
        // malloc's (at least 4 bytes on this 32-bit target) — fine for the
        // Copy/pointer-sized fields CardReadout/ControlPunch actually have,
        // but not a general-purpose allocator for arbitrary over-aligned
        // types.
        debug_assert!(layout.align() <= 4, "PsramAllocator only guarantees 4-byte alignment");

        let ptr = unsafe { heap_caps_malloc(layout.size(), MALLOC_CAP_SPIRAM | MALLOC_CAP_8BIT) } as *mut u8;
        let ptr = NonNull::new(ptr).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return; // nothing was actually allocated for a zero-size layout
        }
        unsafe { heap_caps_free(ptr.as_ptr() as *mut c_void) };
    }
}
