// SPDX-License-Identifier: MIT OR Apache-2.0

use moomap::MooMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

const GIB: u64 = 1024 * 1024 * 1024;
const TIB: u64 = 1024 * GIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    MooMap,

    Mmap,
}

struct Config {
    mode: Mode,

    path: PathBuf,

    file_size: u64,

    io_bytes: u64,

    chunk_size: usize,
}

impl Config {
    fn parse() -> io::Result<Self> {
        let mut mode = Mode::MooMap;
        let mut path = PathBuf::from("moomap-stress.db");
        let mut file_size = 20 * GIB;
        let mut io_bytes = TIB;
        let mut chunk_size = 64 * 1024;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let value = |args: &mut std::iter::Skip<std::env::Args>| {
                args.next().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("missing value for {arg}"),
                    )
                })
            };
            match arg.as_str() {
                "--mode" => {
                    mode = match value(&mut args)?.as_str() {
                        "moomap" => Mode::MooMap,
                        "mmap" => Mode::Mmap,
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("unknown mode {other:?}; expected moomap or mmap"),
                            ));
                        }
                    }
                }
                "--file" => path = value(&mut args)?.into(),
                "--file-size" => file_size = parse_bytes(&value(&mut args)?)?,
                "--io-bytes" => io_bytes = parse_bytes(&value(&mut args)?)?,
                "--chunk-size" => {
                    chunk_size =
                        usize::try_from(parse_bytes(&value(&mut args)?)?).map_err(|_| {
                            io::Error::new(io::ErrorKind::InvalidInput, "chunk too large")
                        })?
                }
                "--help" | "-h" => {
                    println!(
                        "usage: stress [--mode moomap|mmap] [--file PATH] \
                         [--file-size BYTES] [--io-bytes BYTES] [--chunk-size BYTES]\n\
                         suffixes: K, M, G, T; defaults: 20G file, 1T I/O, 64K chunks"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {other:?}"),
                    ));
                }
            }
        }
        if file_size == 0 || chunk_size == 0 || chunk_size as u64 > file_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file and chunk sizes must be nonzero, and chunk must fit in file",
            ));
        }
        Ok(Self {
            mode,
            path,
            file_size,
            io_bytes,
            chunk_size,
        })
    }
}

fn parse_bytes(text: &str) -> io::Result<u64> {
    let (number, multiplier) = match text.as_bytes().last().copied() {
        Some(b'K' | b'k') => (&text[..text.len() - 1], 1024),
        Some(b'M' | b'm') => (&text[..text.len() - 1], 1024 * 1024),
        Some(b'G' | b'g') => (&text[..text.len() - 1], GIB),
        Some(b'T' | b't') => (&text[..text.len() - 1], TIB),
        _ => (text, 1),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|value| value.checked_mul(multiplier))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid byte size {text:?}"),
            )
        })
}

trait Cache {
    fn xor(&self, offset: usize, random: &[u8]) -> io::Result<()>;
    fn write_back(&self) -> io::Result<()>;
    fn reclaim(&self, bytes: usize) -> io::Result<()>;
}

impl Cache for MooMap {
    fn xor(&self, offset: usize, random: &[u8]) -> io::Result<()> {
        self.write_with(offset, random.len(), |mapped| {
            for (target, source) in mapped.iter_mut().zip(random) {
                *target ^= *source;
            }
        })
    }

    fn write_back(&self) -> io::Result<()> {
        let stats = MooMap::write_back(self)?;
        eprintln!(
            "write-back: {} MiB in {} runs",
            stats.bytes / 1024 / 1024,
            stats.runs
        );
        Ok(())
    }

    fn reclaim(&self, bytes: usize) -> io::Result<()> {
        let stats = MooMap::reclaim(self, bytes)?;
        eprintln!(
            "reclaim: {} MiB in {} runs",
            stats.bytes / 1024 / 1024,
            stats.runs
        );
        Ok(())
    }
}

struct StandardMap {
    ptr: NonNull<u8>,

    len: usize,
}

impl StandardMap {
    fn new(file: File, len: usize) -> io::Result<Self> {
        // SAFETY: the descriptor is valid, `len` is the nonzero file length,
        // and the return value is checked before use.
        let raw = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw..raw + len` is the live mapping created above. These
        // calls only change paging policy.
        unsafe {
            libc::madvise(raw, len, libc::MADV_WILLNEED);
            #[cfg(target_os = "linux")]
            libc::madvise(raw, len, libc::MADV_HUGEPAGE);
        }
        Ok(Self {
            ptr: NonNull::new(raw.cast()).unwrap(),
            len,
        })
    }

    fn advise(&self, advice: libc::c_int) -> io::Result<()> {
        // SAFETY: the pointer and length describe the live mapping.
        let result = unsafe { libc::madvise(self.ptr.as_ptr().cast(), self.len, advice) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Cache for StandardMap {
    fn xor(&self, offset: usize, random: &[u8]) -> io::Result<()> {
        // SAFETY: the caller bounds offsets to the mapping, and this benchmark
        // performs mutations from one thread.
        let mapped =
            unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr().add(offset), random.len()) };
        for (target, source) in mapped.iter_mut().zip(random) {
            *target ^= *source;
        }
        Ok(())
    }

    fn write_back(&self) -> io::Result<()> {
        // SAFETY: the pointer and length describe the live shared mapping.
        let result = unsafe { libc::msync(self.ptr.as_ptr().cast(), self.len, libc::MS_ASYNC) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn reclaim(&self, _bytes: usize) -> io::Result<()> {
        // SAFETY: the pointer and length describe the live shared mapping.
        let result = unsafe { libc::msync(self.ptr.as_ptr().cast(), self.len, libc::MS_SYNC) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        self.advise(libc::MADV_DONTNEED)
    }
}

impl Drop for StandardMap {
    fn drop(&mut self) {
        // SAFETY: this is the original mapping and Drop has exclusive access.
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MemoryUsage {
    total: u64,

    available: u64,

    commit_limit: u64,

    committed: u64,
}

impl MemoryUsage {
    fn reclaim_bytes(self, maximum: u64) -> usize {
        fn excess_at_high_water(used: u64, limit: u64) -> u64 {
            if used.saturating_mul(100) < limit.saturating_mul(80) {
                return 0;
            }
            used.saturating_sub(limit.saturating_mul(40) / 100)
        }

        let physical = excess_at_high_water(self.total.saturating_sub(self.available), self.total);
        let commit = excess_at_high_water(self.committed, self.commit_limit);
        usize::try_from(physical.max(commit).min(maximum)).unwrap_or(usize::MAX)
    }
}

fn memory_usage() -> io::Result<MemoryUsage> {
    let info = std::fs::read_to_string("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;
    let mut commit_limit = None;
    let mut committed = None;
    for line in info.lines() {
        let mut fields = line.split_whitespace();
        let key = fields.next();
        let value = fields.next().and_then(|value| value.parse::<u64>().ok());
        match key {
            Some("MemTotal:") => total = value,
            Some("MemAvailable:") => available = value,
            Some("CommitLimit:") => commit_limit = value,
            Some("Committed_AS:") => committed = value,
            _ => {}
        }
    }
    match (total, available, commit_limit, committed) {
        (Some(total), Some(available), Some(commit_limit), Some(committed)) => Ok(MemoryUsage {
            total: total * 1024,
            available: available * 1024,
            commit_limit: commit_limit * 1024,
            committed: committed * 1024,
        }),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "memory or commit counters missing from /proc/meminfo",
        )),
    }
}

fn preallocate(file: &File, size: u64) -> io::Result<()> {
    file.set_len(size)?;
    #[cfg(target_os = "linux")]
    {
        let size = libc::off_t::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file size exceeds off_t"))?;
        // SAFETY: the descriptor is valid and `size` is representable by off_t.
        let result = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, size) };
        if result != 0 {
            let source = io::Error::from_raw_os_error(result);
            return Err(io::Error::new(
                source.kind(),
                format!(
                    "cannot preallocate the benchmark file: {source}; use a filesystem with at least {} GiB free",
                    size as u64 / GIB
                ),
            ));
        }
    }
    Ok(())
}

fn run(config: Config) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&config.path)?;
    preallocate(&file, config.file_size)?;
    let len = usize::try_from(config.file_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file does not fit address space",
        )
    })?;
    let cache: Box<dyn Cache> = match config.mode {
        Mode::MooMap => Box::new(MooMap::from_file(file)?),
        Mode::Mmap => Box::new(StandardMap::new(file, len)?),
    };

    let mut random = File::open("/dev/random")?;
    let mut data = vec![0; config.chunk_size];
    let mut position_bytes = [0; 16];
    let max_offset = config.file_size - config.chunk_size as u64;
    let started = Instant::now();
    let mut last_write_back = Instant::now();
    let mut last_memory_check = Instant::now();
    let mut processed = 0u64;

    while processed < config.io_bytes {
        let amount =
            usize::try_from((config.io_bytes - processed).min(config.chunk_size as u64)).unwrap();
        random.read_exact(&mut data[..amount])?;
        random.read_exact(&mut position_bytes)?;
        let first = u64::from_ne_bytes(position_bytes[..8].try_into().unwrap());
        let second = u64::from_ne_bytes(position_bytes[8..].try_into().unwrap());
        // The minimum of two uniform positions gives a mild, deterministic
        // hot-end bias while every chosen position still comes from /dev/random.
        let offset = if max_offset == 0 {
            0
        } else {
            (first % (max_offset + 1)).min(second % (max_offset + 1))
        };
        cache.xor(offset as usize, &data[..amount])?;
        processed += amount as u64;

        if last_write_back.elapsed() >= Duration::from_secs(5) {
            cache.write_back()?;
            last_write_back = Instant::now();
        }

        if last_memory_check.elapsed() >= Duration::from_millis(250) {
            let bytes = memory_usage()?.reclaim_bytes(config.file_size);
            if bytes != 0 {
                cache.reclaim(bytes)?;
            }
            last_memory_check = Instant::now();
        }

        if processed % GIB < amount as u64 {
            eprintln!(
                "processed {} GiB in {:.1}s",
                processed / GIB,
                started.elapsed().as_secs_f64()
            );
        }
    }

    cache.write_back()?;
    println!(
        "mode={:?} bytes={} seconds={:.3} MiB/s={:.1}",
        config.mode,
        processed,
        started.elapsed().as_secs_f64(),
        processed as f64 / 1024.0 / 1024.0 / started.elapsed().as_secs_f64()
    );
    Ok(())
}

fn main() -> io::Result<()> {
    run(Config::parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_uses_larger_of_physical_and_commit_pressure() {
        let gib = GIB;
        let physical_pressure = MemoryUsage {
            total: 10 * gib,
            available: gib,
            commit_limit: 20 * gib,
            committed: 10 * gib,
        };
        assert_eq!(physical_pressure.reclaim_bytes(20 * gib), 5 * gib as usize);

        let commit_pressure = MemoryUsage {
            total: 10 * gib,
            available: 5 * gib,
            commit_limit: 10 * gib,
            committed: 9 * gib,
        };
        assert_eq!(commit_pressure.reclaim_bytes(20 * gib), 5 * gib as usize);
    }

    #[test]
    fn reclaim_stays_idle_below_both_high_water_marks() {
        let usage = MemoryUsage {
            total: 10 * GIB,
            available: 3 * GIB,
            commit_limit: 10 * GIB,
            committed: 7 * GIB,
        };
        assert_eq!(usage.reclaim_bytes(20 * GIB), 0);
    }
}
