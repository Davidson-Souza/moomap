// SPDX-License-Identifier: MIT OR Apache-2.0
#![deny(missing_docs)]

//! MooMap keeps file-backed pages resident as private copy-on-write pages.
//!
//! Writes must go through [`MooMap::write_with`] or [`MooMap::write`] so dirty
//! pages can be tracked. `write_back` deliberately does not synchronize with
//! writers: it is a best-effort snapshot. `reclaim` takes a per-page writer
//! gate, writes each selected run, and discards its private pages. Readers that
//! use [`MooMap::as_ptr`] are never gated.
//!
//! # Example
//!
//! ```no_run
//! use moomap::MooMap;
//! use std::fs::OpenOptions;
//! use std::io;
//!
//! fn main() -> io::Result<()> {
//!     let file = OpenOptions::new()
//!         .create(true)
//!         .truncate(true)
//!         .read(true)
//!         .write(true)
//!         .open("example.db")?;
//!     file.set_len(1024 * 1024)?;
//!
//!     let map = MooMap::from_file(file)?;
//!     map.write(0, b"moo")?;
//!     map.write_back()?;
//!     Ok(())
//! }
//! ```

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const IDLE: u8 = 0;
const WRITING: u8 = 1;
const RECLAIMING: u8 = 2;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
/// Counts the work completed by a write-back or reclaim operation.
pub struct IoStats {
    /// Number of base pages processed.
    pub pages: usize,

    /// Number of contiguous page runs processed.
    pub runs: usize,

    /// Number of mapped bytes processed.
    pub bytes: usize,
}

#[derive(Debug)]
struct PageState {
    dirty: AtomicBool,

    access: AtomicU8,
}

impl PageState {
    fn new() -> Self {
        Self {
            dirty: AtomicBool::new(false),
            access: AtomicU8::new(IDLE),
        }
    }
}

/// A writable, private mapping of an entire file.
///
/// The file remains the durable source of truth. Private CoW pages stay
/// resident after [`write_back`](Self::write_back), and are returned to the
/// kernel only by [`reclaim`](Self::reclaim).
pub struct MooMap {
    ptr: NonNull<u8>,

    len: usize,

    page_size: usize,

    file: File,

    pages: Box<[PageState]>,
}

// SAFETY: writer gates serialize safe mutable access, the mapping address is
// stable for the object's lifetime, and raw-pointer users must synchronize
// conflicting accesses before dereferencing.
unsafe impl Send for MooMap {}
unsafe impl Sync for MooMap {}

impl MooMap {
    /// Opens and privately maps an existing file for reading and writing.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened or mapped, is empty, or
    /// is larger than the process address space.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::from_file(file)
    }

    /// Privately maps an existing, writable file.
    ///
    /// The mapping takes ownership of `file` and spans its current length.
    ///
    /// # Errors
    ///
    /// Returns an error when the file metadata cannot be read, the file is
    /// empty, or the operating system rejects the mapping.
    pub fn from_file(file: File) -> io::Result<Self> {
        let file_len = file.metadata()?.len();
        let len = usize::try_from(file_len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "file does not fit address space",
            )
        })?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot map an empty file",
            ));
        }

        // SAFETY: sysconf has no pointer arguments or caller-owned memory.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(io::Error::last_os_error());
        }
        let page_size = page_size as usize;
        let map_flags = libc::MAP_PRIVATE;
        // A writable private mapping is otherwise charged as though every
        // page might become anonymous CoW memory. That can reject files larger
        // than the commit limit before a page is touched. Reclaim provides the
        // runtime bound, so Linux should not reserve the whole mapping.
        #[cfg(target_os = "linux")]
        let map_flags = map_flags | libc::MAP_NORESERVE;
        // SAFETY: the file descriptor remains owned by MooMap, `len` is its
        // nonzero current length, and the return value is checked before use.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                map_flags,
                file.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(raw.cast()).expect("mmap returned a null success address");

        // Transparent huge pages need no reserved hugetlb pool and can fall
        // back to base pages. Advice failure is non-fatal on kernels/filesystems
        // that cannot provide huge pages for this mapping.
        #[cfg(target_os = "linux")]
        // SAFETY: `raw..raw + len` is the live mapping created above. This
        // advice changes only the kernel's paging policy.
        unsafe {
            libc::madvise(raw, len, libc::MADV_HUGEPAGE);
        }

        let page_count = len.div_ceil(page_size);
        let pages = (0..page_count)
            .map(|_| PageState::new())
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok(Self {
            ptr,
            len,
            page_size,
            file,
            pages,
        })
    }

    /// Returns the mapped file length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the mapping is empty.
    ///
    /// MooMap rejects empty files, so this always returns `false`.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Returns the operating system's base-page size.
    pub fn page_size(&self) -> usize {
        self.page_size
    }

    /// Returns the stable base address of the mapping.
    ///
    /// Dereferencing it is unsafe. In particular, callers must synchronize
    /// reads that overlap concurrent writes. Reclaim never invalidates the
    /// address and never waits for readers.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr()
    }

    /// Mutates a byte range while holding only that range's per-page writer gates.
    ///
    /// # Errors
    ///
    /// Returns an error when `offset..offset + len` exceeds the mapping.
    pub fn write_with<R>(
        &self,
        offset: usize,
        len: usize,
        mutate: impl FnOnce(&mut [u8]) -> R,
    ) -> io::Result<R> {
        let end = offset
            .checked_add(len)
            .filter(|&end| end <= self.len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "write out of bounds"))?;
        if len == 0 {
            return Ok(mutate(&mut []));
        }

        let first = offset / self.page_size;
        let last = (end - 1) / self.page_size;
        let guard = WriteGuard::acquire(&self.pages, first, last);
        // SAFETY: bounds were checked above and the page gates exclude every
        // overlapping safe writer for the lifetime of the mutable slice.
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), len) };
        let result = mutate(bytes);
        drop(guard);
        Ok(result)
    }

    /// Copies `bytes` into the mapping at `offset`.
    ///
    /// # Errors
    ///
    /// Returns an error when the destination range exceeds the mapping.
    pub fn write(&self, offset: usize, bytes: &[u8]) -> io::Result<()> {
        self.write_with(offset, bytes.len(), |target| target.copy_from_slice(bytes))
    }

    /// Copies dirty runs into the backing file without locking writers.
    ///
    /// The copy is intentionally a best-effort snapshot. A concurrent writer
    /// may be observed partly, and dirty flags remain set so a later call can
    /// refresh the file. `pwrite` updates the page cache; durable media flushes
    /// remain the consumer's responsibility.
    ///
    /// This method performs `pwrite` calls on the caller's thread; asynchronous
    /// describes its relationship with concurrent writers, not a nonblocking API.
    ///
    /// # Errors
    ///
    /// Returns the first backing-file write error.
    pub fn write_back(&self) -> io::Result<IoStats> {
        let mut stats = IoStats::default();
        let mut page = 0;
        while page < self.pages.len() {
            if !self.pages[page].dirty.load(Ordering::Acquire) {
                page += 1;
                continue;
            }
            let start = page;
            page += 1;
            while page < self.pages.len() && self.pages[page].dirty.load(Ordering::Acquire) {
                page += 1;
            }
            let offset = start * self.page_size;
            let end = (page * self.page_size).min(self.len);
            self.pwrite_range(offset, end - offset)?;
            stats.pages += page - start;
            stats.runs += 1;
            stats.bytes += end - offset;
        }
        Ok(stats)
    }

    /// Writes and discards up to `max_bytes` of dirty private pages.
    ///
    /// A per-page access state marks reclaim ownership. It blocks writers only
    /// for pages in the current run; unrelated writers and every reader
    /// continue without a global lock.
    ///
    /// # Errors
    ///
    /// Returns the first backing-file write or `madvise` error.
    pub fn reclaim(&self, max_bytes: usize) -> io::Result<IoStats> {
        if max_bytes == 0 {
            return Ok(IoStats::default());
        }
        let page_limit = max_bytes.div_ceil(self.page_size);
        let mut stats = IoStats::default();
        let mut page = 0;

        while page < self.pages.len() && stats.pages < page_limit {
            if !self.try_gate(page) {
                page += 1;
                continue;
            }
            if !self.pages[page].dirty.load(Ordering::Acquire) {
                self.ungate(page);
                page += 1;
                continue;
            }

            let start = page;
            page += 1;
            while page < self.pages.len()
                && stats.pages + (page - start) < page_limit
                && self.pages[page].dirty.load(Ordering::Acquire)
                && self.try_gate(page)
            {
                page += 1;
            }
            let offset = start * self.page_size;
            let end = (page * self.page_size).min(self.len);
            let len = end - offset;

            if let Err(error) = self.pwrite_range(offset, len) {
                self.ungate_run(start, page);
                return Err(error);
            }
            // SAFETY: the range lies within the live mapping. Writer gates are
            // held for every page in the range until madvise returns.
            let advised = unsafe {
                libc::madvise(
                    self.ptr.as_ptr().add(offset).cast(),
                    len,
                    libc::MADV_DONTNEED,
                )
            };
            if advised != 0 {
                let error = io::Error::last_os_error();
                self.ungate_run(start, page);
                return Err(error);
            }

            for state in &self.pages[start..page] {
                state.dirty.store(false, Ordering::Release);
            }
            self.ungate_run(start, page);
            stats.pages += page - start;
            stats.runs += 1;
            stats.bytes += len;
        }
        Ok(stats)
    }

    /// Writes and discards every dirty private page available for reclaim.
    ///
    /// # Errors
    ///
    /// Returns the first backing-file write or `madvise` error.
    pub fn reclaim_all(&self) -> io::Result<IoStats> {
        self.reclaim(usize::MAX)
    }

    fn try_gate(&self, page: usize) -> bool {
        self.pages[page]
            .access
            .compare_exchange(IDLE, RECLAIMING, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    fn ungate(&self, page: usize) {
        self.pages[page].access.store(IDLE, Ordering::Release);
    }

    fn ungate_run(&self, start: usize, end: usize) {
        for page in start..end {
            self.ungate(page);
        }
    }

    fn pwrite_range(&self, offset: usize, len: usize) -> io::Result<()> {
        let mut written = 0;
        while written < len {
            // SAFETY: the source range lies within the live mapping, and the
            // backing file descriptor remains open for MooMap's lifetime.
            let result = unsafe {
                libc::pwrite(
                    self.file.as_raw_fd(),
                    self.ptr.as_ptr().add(offset + written).cast(),
                    len - written,
                    (offset + written) as libc::off_t,
                )
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pwrite returned zero",
                ));
            }
            written += result as usize;
        }
        Ok(())
    }
}

impl Drop for MooMap {
    fn drop(&mut self) {
        // SAFETY: this is the original live mapping, and Drop has exclusive
        // access to MooMap so no safe operation can still use it.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

struct WriteGuard<'a> {
    pages: &'a [PageState],
    first: usize,
    last: usize,
}

impl<'a> WriteGuard<'a> {
    fn acquire(pages: &'a [PageState], first: usize, last: usize) -> Self {
        for state in &pages[first..=last] {
            while state
                .access
                .compare_exchange_weak(IDLE, WRITING, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }
        }
        Self { pages, first, last }
    }
}

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        for state in &self.pages[self.first..=self.last] {
            state.dirty.store(true, Ordering::Release);
            state.access.store(IDLE, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_file(size: usize) -> (std::path::PathBuf, File) {
        let name = format!(
            "moomap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let path = std::env::temp_dir().join(name);
        let mut file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(size as u64).unwrap();
        file.write_all(&vec![0x11; size]).unwrap();
        (path, file)
    }

    fn bytes_at(file: &mut File, offset: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn write_back_updates_file_but_keeps_private_page() {
        let (path, mut file) = temporary_file(8192);
        let map = MooMap::open(&path).unwrap();
        map.write(100, &[1, 2, 3, 4]).unwrap();
        assert_eq!(bytes_at(&mut file, 100, 4), [0x11; 4]);

        let stats = map.write_back().unwrap();
        assert_eq!(stats.pages, 1);
        assert_eq!(bytes_at(&mut file, 100, 4), [1, 2, 3, 4]);
        // SAFETY: the tested range is in bounds and no writer is active.
        let mapped = unsafe { std::slice::from_raw_parts(map.as_ptr().add(100), 4) };
        assert_eq!(mapped, [1, 2, 3, 4]);

        drop(map);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reclaim_writes_and_discards_private_pages() {
        let (path, mut file) = temporary_file(8192);
        let map = MooMap::open(&path).unwrap();
        map.write(4090, &[7; 20]).unwrap();

        let stats = map.reclaim_all().unwrap();
        assert_eq!(stats.pages, 2);
        assert_eq!(bytes_at(&mut file, 4090, 20), [7; 20]);
        // SAFETY: the tested range is in bounds and no writer is active.
        let mapped = unsafe { std::slice::from_raw_parts(map.as_ptr().add(4090), 20) };
        assert_eq!(mapped, [7; 20]);
        assert_eq!(map.reclaim_all().unwrap(), IoStats::default());

        drop(map);
        std::fs::remove_file(path).unwrap();
    }
}
