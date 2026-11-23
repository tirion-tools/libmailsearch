//! Windows counterparts of the reader's few POSIX calls: positional reads (`pread`), dropping a mapping's pages
//! from the process (`madvise(MADV_DONTNEED)`) and the prefetch (`posix_fadvise` + `mincore`, neither of which has a
//! Windows equivalent for the file cache).

use std::io;
use std::path::PathBuf;

/// `read_at` / `read_exact_at` over `std::os::windows::fs::FileExt::seek_read`, which also moves the file cursor;
/// the reader's descriptors are only ever read positionally.
pub trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl FileExt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }

    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn VirtualUnlock(address: *mut core::ffi::c_void, size: usize) -> i32;
}

/// Removes the pages of one live read-only file mapping from the process's working set: `VirtualUnlock` on a range
/// that is not locked does exactly that (and reports `ERROR_NOT_LOCKED`, the expected result, ignored). Later reads
/// fault the pages back in from the file cache, as after `MADV_DONTNEED` on Linux.
pub fn release(m: &memmap2::Mmap) {
    // SAFETY: the range is exactly one live mapping held by `m`; VirtualUnlock neither reads nor writes it.
    unsafe { VirtualUnlock(m.as_ptr() as *mut core::ffi::c_void, m.len()) };
}

/// [`crate::engine::Engine::prefetch`]'s thread on Windows: reads every file once through its descriptor, so its
/// pages are in the file cache (a warm prefetch copies from the cache: Windows has no `mincore` for it). Errors are
/// ignored (a file gone after a reload is no longer needed).
pub fn prefetch_files(files: &[(PathBuf, u64)]) {
    let mut buf = vec![0u8; 256 << 10];
    for (p, n) in files.iter().filter(|(_, n)| *n > 0) {
        let Ok(f) = std::fs::File::open(p) else {
            continue;
        };
        let mut o = 0u64;
        while o < *n {
            let k = ((*n - o) as usize).min(buf.len());
            match f.read_at(&mut buf[..k], o) {
                Ok(0) => break,
                Ok(r) => o += r as u64,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
    }
}
