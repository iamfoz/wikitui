//! PRD §6.8 `low_memory`: give freed heap back to the operating system.
//!
//! Dropping a dehydrated tab's parse (`App::enforce_residency`) frees its
//! memory to the allocator, not to the OS — and the bigger cost is the parse
//! itself: html5ever's DOM for a 1.5 MB article peaks at several times the
//! size of the `Document` it leaves behind, and glibc's malloc keeps those
//! freed pages mapped (fragmentation stops it shrinking the heap top), so
//! resident memory ratchets up to the high-water mark of the biggest parse
//! and stays there. `malloc_trim(0)` walks every arena and `madvise`s whole
//! free pages back to the kernel, which is what turns "the parse is freed"
//! into "RSS actually drops". Measured on the 10-tab large-article fixture
//! (`tests/mock-server/large_pages.py`), see `App::enforce_residency`'s
//! callers for when it runs: only after something was actually dropped or
//! re-parsed, never per frame.
//!
//! glibc-only (`target_env = "gnu"`); everywhere else — musl, macOS,
//! Windows, whose allocators return memory on their own schedules — this is
//! a no-op, and low-memory mode still saves what it saves by not keeping the
//! parses at all.

/// Returns free heap pages to the OS where the allocator supports it (glibc
/// `malloc_trim(0)`); a no-op elsewhere. Cheap enough to call after each
/// tab dehydrate/rehydrate (it walks the free lists; ~1 ms on a 50 MB heap).
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn release_free_memory() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: `malloc_trim` is a thread-safe glibc entry point with no
    // preconditions; `pad = 0` asks it to keep no slack at the heap top.
    unsafe {
        malloc_trim(0);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn release_free_memory() {}

#[cfg(test)]
mod tests {
    #[test]
    fn release_free_memory_is_safe_to_call_repeatedly() {
        let big: Vec<u8> = vec![7; 8 << 20];
        drop(big);
        super::release_free_memory();
        super::release_free_memory();
    }
}
