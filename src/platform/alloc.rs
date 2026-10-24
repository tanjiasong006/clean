//! Global allocator for macOS. The system allocator keeps freed pages resident,
//! so replacing the results of a multi-million entry scan leaves the previous
//! scan's arenas in the process footprint. Blocks of at least `MIN_MMAP` bytes
//! are backed by anonymous mappings instead, which the kernel reclaims as soon
//! as they are freed. Smaller blocks stay with the system allocator, where the
//! mapping overhead would outweigh the benefit.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Scan arenas grow in 1 MiB name chunks and multi-megabyte node tables; blocks
/// below this size are left to the system allocator.
const MIN_MMAP: usize = 1 << 20;

pub struct LargeAllocator;

fn page_size() -> usize {
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    // Cache the result so the common path avoids a syscall.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let size = if size > 0 { size as usize } else { 4096 };
    CACHED.store(size, Ordering::Relaxed);
    size
}

fn mapped_size(size: usize) -> usize {
    let page = page_size();
    size.div_ceil(page) * page
}

/// Large blocks are mapped only when the mapping's alignment is sufficient;
/// over-aligned requests stay with the system allocator.
fn uses_mmap(layout: &Layout) -> bool {
    layout.size() >= MIN_MMAP && layout.align() <= page_size()
}

unsafe fn map(size: usize) -> *mut u8 {
    // SAFETY: an anonymous private mapping has no file backing and returns a
    // fresh, zero-filled, page-aligned range.
    let pointer = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        )
    };
    if pointer == libc::MAP_FAILED {
        std::ptr::null_mut()
    } else {
        pointer.cast()
    }
}

unsafe fn unmap(pointer: *mut u8, size: usize) {
    // SAFETY: the caller passes a pointer returned by `map` with the same
    // layout, so the range is still mapped.
    unsafe {
        libc::munmap(pointer.cast(), mapped_size(size));
    }
}

unsafe impl GlobalAlloc for LargeAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if uses_mmap(&layout) {
            unsafe { map(mapped_size(layout.size())) }
        } else {
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if uses_mmap(&layout) {
            // Anonymous mappings are already zeroed.
            unsafe { map(mapped_size(layout.size())) }
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if uses_mmap(&layout) {
            unsafe { unmap(pointer, layout.size()) };
        } else {
            unsafe { System.dealloc(pointer, layout) };
        }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: `new_size` is what the caller will use, and alignment is kept.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        match (uses_mmap(&layout), uses_mmap(&new_layout)) {
            (true, true) => {
                let block = unsafe { map(mapped_size(new_size)) };
                if block.is_null() {
                    return std::ptr::null_mut();
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(pointer, block, layout.size().min(new_size));
                    unmap(pointer, layout.size());
                }
                block
            }
            (true, false) => {
                let block = unsafe { System.alloc(new_layout) };
                if block.is_null() {
                    return std::ptr::null_mut();
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(pointer, block, new_size);
                    unmap(pointer, layout.size());
                }
                block
            }
            (false, true) => {
                let block = unsafe { map(mapped_size(new_size)) };
                if block.is_null() {
                    return std::ptr::null_mut();
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(pointer, block, layout.size());
                    System.dealloc(pointer, layout);
                }
                block
            }
            (false, false) => unsafe { System.realloc(pointer, layout, new_size) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(size: usize) -> Layout {
        Layout::from_size_align(size, 8).unwrap()
    }

    #[test]
    fn large_blocks_keep_contents_across_growth_and_shrink() {
        let allocator = LargeAllocator;
        // SAFETY: every pointer is checked and used within the requested size.
        unsafe {
            let first = layout(MIN_MMAP);
            let block = allocator.alloc(first);
            assert!(!block.is_null());
            std::ptr::write_bytes(block, 7, first.size());

            let grown = allocator.realloc(block, first, 3 * MIN_MMAP);
            assert!(!grown.is_null());
            assert!((0..first.size()).all(|index| *grown.add(index) == 7));

            let shrunk = allocator.realloc(grown, layout(3 * MIN_MMAP), 1024);
            assert!(!shrunk.is_null());
            assert!((0..1024).all(|index| *shrunk.add(index) == 7));
            allocator.dealloc(shrunk, layout(1024));
        }
    }

    #[test]
    fn reallocation_moves_blocks_across_the_threshold() {
        let allocator = LargeAllocator;
        // SAFETY: every pointer is checked and used within the requested size.
        unsafe {
            let small = layout(MIN_MMAP / 2);
            let block = allocator.alloc(small);
            assert!(!block.is_null());
            std::ptr::write_bytes(block, 3, small.size());

            let grown = allocator.realloc(block, small, 2 * MIN_MMAP);
            assert!(!grown.is_null());
            assert!((0..small.size()).all(|index| *grown.add(index) == 3));

            let shrunk = allocator.realloc(grown, layout(2 * MIN_MMAP), small.size());
            assert!(!shrunk.is_null());
            assert!((0..small.size()).all(|index| *shrunk.add(index) == 3));
            allocator.dealloc(shrunk, small);
        }
    }

    #[test]
    fn fresh_large_blocks_are_zeroed() {
        let allocator = LargeAllocator;
        let block = unsafe { allocator.alloc_zeroed(layout(2 * MIN_MMAP)) };
        assert!(!block.is_null());
        // SAFETY: the block is 2 MiB and initialized by the allocator.
        unsafe {
            assert!((0..2 * MIN_MMAP).all(|index| *block.add(index) == 0));
            allocator.dealloc(block, layout(2 * MIN_MMAP));
        }
    }
}
