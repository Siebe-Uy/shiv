//! A header declaring a huge directory on a small file must be rejected from
//! the declared sizes, never by allocating a buffer of that size.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use vpk_parser::VpkParser;

/// Records the largest single allocation request (this binary has one test).
struct Peak;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Peak {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Peak = Peak;

#[test]
fn max_tree_length_on_a_small_file_allocates_nothing_large() {
    let dir = std::env::temp_dir().join(format!("vpk-parser-huge-tree-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("pak01_dir.vpk");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0x55aa_1234u32.to_le_bytes());
    bytes.extend_from_slice(&2u32.to_le_bytes());
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // tree_length
    bytes.extend_from_slice(&[0u8; 16]);
    bytes.extend_from_slice(b"txt\0\0\0");
    std::fs::write(&path, &bytes).unwrap();
    LARGEST.store(0, Ordering::Relaxed);

    let header = VpkParser::header_from_file(&path).unwrap();
    assert_eq!(header.tree_length, u32::MAX);
    assert!(VpkParser::parse_directory_from_file(&path).is_err());
    assert!(VpkParser::parse_directory_with_preload_from_file(&path).is_err());

    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(largest < 1 << 20, "largest allocation was {largest} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}
