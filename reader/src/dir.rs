//! The reader's own read-only Tantivy directory: every file is mapped once and registered, so [`RoDir::advise_all`] can
//! `madvise(MADV_DONTNEED)` every live mapping and drop its pages from this process. Writes, deletes
//! and locks are refused: the indexer is the only writer. A segment file Tantivy decodes at open
//! (`mailsearch_core::integrity`) is checked against its footer CRC when it is first mapped; a
//! mismatch fails the open and is remembered ([`RoDir::corrupt`]).

use std::collections::HashMap;
use std::io;
use std::ops::{Deref, Range};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use tantivy::Directory;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    DirectoryLock, FileHandle, FileSlice, Lock, OwnedBytes, WatchCallback, WatchHandle, WritePtr,
};

struct MapArc(Arc<memmap2::Mmap>);
impl Deref for MapArc {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0[..]
    }
}
// SAFETY: the bytes live in the mapping the Arc keeps alive; moving the Arc does not move them.
unsafe impl stable_deref_trait::StableDeref for MapArc {}

#[derive(Debug)]
struct MapHandle(Arc<memmap2::Mmap>);
impl tantivy::HasLen for MapHandle {
    fn len(&self) -> usize {
        self.0.len()
    }
}
impl FileHandle for MapHandle {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        if range.end > self.0.len() || range.start > range.end {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of an index file",
            ));
        }
        Ok(OwnedBytes::new(MapArc(self.0.clone())).slice(range))
    }
}

/// A file read with `pread` (the open check): its pages go to the page cache only, never into this
/// process's mappings, so they do not count in its memory until a query touches them.
#[derive(Debug)]
struct PreadHandle {
    f: std::fs::File,
    len: usize,
}
impl tantivy::HasLen for PreadHandle {
    fn len(&self) -> usize {
        self.len
    }
}
impl FileHandle for PreadHandle {
    fn read_bytes(&self, range: Range<usize>) -> io::Result<OwnedBytes> {
        if range.end > self.len || range.start > range.end {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read past the end of an index file",
            ));
        }
        let mut b = vec![0u8; range.len()];
        self.f.read_exact_at(&mut b, range.start as u64)?;
        Ok(OwnedBytes::new(b))
    }
}

#[derive(Clone, Debug)]
pub struct RoDir {
    root: PathBuf,
    maps: Arc<Mutex<HashMap<PathBuf, Weak<memmap2::Mmap>>>>,
    corrupt: Arc<Mutex<Option<String>>>,
}

fn read_err(p: &Path, e: io::Error) -> OpenReadError {
    if e.kind() == io::ErrorKind::NotFound {
        OpenReadError::FileDoesNotExist(p.to_path_buf())
    } else {
        OpenReadError::IoError {
            io_error: Arc::new(e),
            filepath: p.to_path_buf(),
        }
    }
}

fn denied() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "read-only reader")
}

impl RoDir {
    pub fn open(root: &Path) -> io::Result<RoDir> {
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not a directory", root.display()),
            ));
        }
        Ok(RoDir {
            root: root.to_path_buf(),
            maps: Default::default(),
            corrupt: Default::default(),
        })
    }

    /// The first file that failed its integrity check, with the reason.
    pub fn corrupt(&self) -> Option<String> {
        self.corrupt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `madvise(MADV_DONTNEED)` on every live mapping. Returns the bytes advised.
    pub fn advise_all(&self) -> u64 {
        let mut n = 0u64;
        let mut maps = self.maps.lock().unwrap_or_else(|e| e.into_inner());
        maps.retain(|_, w| w.strong_count() > 0);
        for w in maps.values() {
            if let Some(m) = w.upgrade()
                && !m.is_empty()
            {
                // SAFETY: the range is exactly one live read-only file mapping held by `m`;
                // MADV_DONTNEED on a file-backed private read-only mapping only drops pages,
                // later reads fault them back in from the file.
                unsafe {
                    libc::madvise(
                        m.as_ptr() as *mut libc::c_void,
                        m.len(),
                        libc::MADV_DONTNEED,
                    )
                };
                n += m.len() as u64;
            }
        }
        n
    }

    /// The files of the live mappings whose extension is one of `exts`, grouped in the order of `exts`, with
    /// their lengths.
    pub fn mapped_files(&self, exts: &[&str]) -> Vec<(PathBuf, u64)> {
        let maps = self.maps.lock().unwrap_or_else(|e| e.into_inner());
        let mut v: Vec<(usize, PathBuf, u64)> = maps
            .iter()
            .filter_map(|(p, w)| {
                let m = w.upgrade()?;
                let ext = p.extension()?.to_str()?;
                let rank = exts.iter().position(|e| *e == ext)?;
                Some((rank, self.root.join(p), m.len() as u64))
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, p, n)| (p, n)).collect()
    }
}

impl Directory for RoDir {
    fn get_file_handle(&self, p: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        let mut maps = self.maps.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(m) = maps.get(p).and_then(|w| w.upgrade()) {
            return Ok(Arc::new(MapHandle(m)));
        }
        let f = std::fs::File::open(self.root.join(p)).map_err(|e| read_err(p, e))?;
        let len = f.metadata().map_err(|e| read_err(p, e))?.len();
        let m = if len == 0 {
            Arc::new(
                memmap2::MmapOptions::new()
                    .len(0)
                    .map_anon()
                    .and_then(|m| m.make_read_only())
                    .map_err(|e| read_err(p, e))?,
            )
        } else {
            // SAFETY: index files are immutable once written; the indexer never truncates a file
            // a reader generation may hold (lease).
            Arc::new(unsafe { memmap2::Mmap::map(&f) }.map_err(|e| read_err(p, e))?)
        };
        // The check reads through the descriptor, not the mapping: reading the store, fast
        // fields, fieldnorms and deletes through it kept every page mapped (76 MiB at 1M) until the
        // first release, though a query touches few of them.
        if mailsearch_core::integrity::checked_at_open(p)
            && let Err(e) = f.try_clone().map_err(|e| e.to_string()).and_then(|f| {
                let h = PreadHandle {
                    f,
                    len: len as usize,
                };
                mailsearch_core::integrity::check_file(FileSlice::new(Arc::new(h)))
            })
        {
            let msg = format!("{}: {e}", p.display());
            self.corrupt
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| msg.clone());
            return Err(read_err(p, io::Error::new(io::ErrorKind::InvalidData, msg)));
        }
        maps.insert(p.to_path_buf(), Arc::downgrade(&m));
        Ok(Arc::new(MapHandle(m)))
    }
    fn delete(&self, p: &Path) -> Result<(), DeleteError> {
        Err(DeleteError::IoError {
            io_error: Arc::new(denied()),
            filepath: p.to_path_buf(),
        })
    }
    fn exists(&self, p: &Path) -> Result<bool, OpenReadError> {
        Ok(self.root.join(p).exists())
    }
    fn open_write(&self, p: &Path) -> Result<WritePtr, OpenWriteError> {
        Err(OpenWriteError::IoError {
            io_error: Arc::new(denied()),
            filepath: p.to_path_buf(),
        })
    }
    fn atomic_read(&self, p: &Path) -> Result<Vec<u8>, OpenReadError> {
        std::fs::read(self.root.join(p)).map_err(|e| read_err(p, e))
    }
    fn atomic_write(&self, _p: &Path, _data: &[u8]) -> io::Result<()> {
        Err(denied())
    }
    fn sync_directory(&self) -> io::Result<()> {
        Ok(())
    }
    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }
    fn watch(&self, _cb: WatchCallback) -> tantivy::Result<WatchHandle> {
        Ok(WatchHandle::empty())
    }
}
