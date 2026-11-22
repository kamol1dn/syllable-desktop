//! Debug aid (`--features guard-heap`, then `SYLLABLE_GUARD_HEAP=1`) puts every Rust allocation at the
//! end of its own pages with an inaccessible page after it, and decommits it on free.
//! A write past the end of a Rust buffer, or into one after it was freed, then faults
//! at the guilty instruction instead of corrupting the heap silently. Like page heap,
//! without needing admin rights for gflags. Off by default: the system allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use windows::core::w;
use windows::Win32::System::Environment::GetEnvironmentVariableW;
use windows::Win32::System::Memory::{VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, PAGE_NOACCESS};

const PAGE: usize = 4096;
/// Freed blocks stay decommitted (use-after-free faults) until this many newer frees.
const QUARANTINE: usize = 1 << 18;

pub struct GuardHeap;

static MODE: AtomicU8 = AtomicU8::new(0); // 0 = not decided, 1 = system, 2 = guard
static LOCK: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static mut RING: [usize; QUARANTINE] = [0; QUARANTINE];

fn guard() -> bool {
    match MODE.load(Ordering::Relaxed) {
        1 => false,
        2 => true,
        _ => {
            let mut buf = [0u16; 8];
            let n = unsafe { GetEnvironmentVariableW(w!("SYLLABLE_GUARD_HEAP"), Some(&mut buf)) };
            let on = n == 1 && buf[0] == b'1' as u16;
            MODE.store(if on { 2 } else { 1 }, Ordering::Relaxed);
            on
        }
    }
}

fn data_pages(layout: Layout) -> (usize, usize) {
    let size = layout.size().max(1);
    let rsize = size.div_ceil(layout.align()) * layout.align();
    (rsize, rsize.div_ceil(PAGE))
}

unsafe impl GlobalAlloc for GuardHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !guard() || layout.align() > PAGE {
            return System.alloc(layout);
        }
        let (rsize, pages) = data_pages(layout);
        let base = VirtualAlloc(None, (pages + 1) * PAGE, MEM_RESERVE, PAGE_NOACCESS);
        if base.is_null() {
            return std::ptr::null_mut();
        }
        if VirtualAlloc(Some(base), pages * PAGE, MEM_COMMIT, PAGE_READWRITE).is_null() {
            let _ = VirtualFree(base, 0, MEM_RELEASE);
            return std::ptr::null_mut();
        }
        (base as *mut u8).add(pages * PAGE - rsize)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if !guard() || layout.align() > PAGE {
            return System.dealloc(ptr, layout);
        }
        let (_, pages) = data_pages(layout);
        let base = (ptr as usize) & !(PAGE - 1);
        let _ = VirtualFree(base as *mut _, pages * PAGE, MEM_DECOMMIT);
        while LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        let i = NEXT.fetch_add(1, Ordering::Relaxed) % QUARANTINE;
        let ring = &raw mut RING;
        let old = (*ring)[i];
        (*ring)[i] = base;
        LOCK.store(false, Ordering::Release);
        if old != 0 {
            let _ = VirtualFree(old as *mut _, 0, MEM_RELEASE);
        }
    }
}
