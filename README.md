<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

<p align="center">
  <img src="assets/moomap.svg" alt="MooMap: a cow emerging from a memory module" width="424">
</p>

# MooMap

MooMap is an experimental Rust wrapper around private, writable file mappings. It keeps modified pages as anonymous copy-on-write (CoW) memory, writes them back to the real file in contiguous runs, and can later return selected pages to the kernel without stopping unrelated writers.

The name is literal: it is a CoW map with no global herd lock.

> [!WARNING]
> MooMap is an experiment, not a crash-consistent storage engine. `write_back` is deliberately best-effort, and the caller remains responsible for synchronization and durable flushes.

## Why

A normal shared mapping lets the kernel write file-backed pages out and evict them. That is usually desirable, but it can cause repeated major faults when an application has enough RAM and wants to retain a large, hot working set.

MooMap instead uses `MAP_PRIVATE`. The first write to a page creates a private CoW page. That page is no longer ordinary clean file-cache data, so it remains available until memory pressure swaps it or the application explicitly reclaims it.

## How it works

- The entire non-empty file is mapped with `PROT_READ | PROT_WRITE` and `MAP_PRIVATE`.
- Linux mappings use `MAP_NORESERVE`, allowing the mapped file to exceed the current commit limit without reserving memory for every possible CoW page.
- Linux receives `MADV_HUGEPAGE` advice. Transparent huge pages remain advisory; the kernel may continue using base pages.
- Writes through `write` or `write_with` mark the affected base pages dirty.
- `write_back` scans dirty pages in address order and uses `pwrite` once per contiguous run. It does not lock writers and does not discard private pages.
- `reclaim` takes a small per-page writer gate, writes a contiguous run, calls `MADV_DONTNEED`, and releases the gate. Readers and writers touching other pages continue.

`MADV_DONTNEED` is intentional. There is no portable `MADV_WONTNEED`, and `msync` cannot write private CoW data back to the source file.

## Library usage

```rust
use moomap::MooMap;
use std::fs::OpenOptions;
use std::io;

fn main() -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open("example.db")?;
    file.set_len(16 * 1024 * 1024)?;

    // Keep a descriptor for the consumer-controlled durable flush.
    let durable_file = file.try_clone()?;
    let map = MooMap::from_file(file)?;

    map.write(4096, b"moo")?;

    // Best-effort copy into the kernel's file cache. This call performs pwrite
    // on the current thread but does not synchronize with concurrent writers.
    let copied = map.write_back()?;
    println!("copied {} bytes in {} runs", copied.bytes, copied.runs);

    // A strong flush requires the application to exclude writers first.
    durable_file.sync_all()?;

    // Write and discard up to 64 MiB of dirty private pages.
    map.reclaim(64 * 1024 * 1024)?;
    Ok(())
}
```

### Concurrency contract

`write` and `write_with` serialize writers only when their page ranges overlap. Reclaim uses the same per-page state, so it cannot discard a page between that page's write-back and `MADV_DONTNEED`. There is no map-wide mutex.

`write_back` intentionally takes no writer gates. A racing write may be copied partially; dirty pages stay marked so a later write-back can refresh the file. This is suitable for periodic best-effort copying, not a transaction boundary.

`as_ptr` returns a stable raw address and reclaim does not wait for readers. Dereferencing that pointer is unsafe: the caller must prevent Rust data races between raw reads and overlapping writes.

### Durability

`pwrite` updates the kernel page cache; it does not guarantee durable media. For a strong flush:

1. Exclude application writers.
2. Call `write_back`.
3. Call `File::sync_all` on a retained or newly opened descriptor.
4. Release the application writer exclusion.

MooMap does not provide the application-level exclusion because imposing one would defeat its concurrency model.

### Files larger than RAM

The virtual mapping may be larger than physical RAM. Only touched private pages consume anonymous memory. On Linux, `MAP_NORESERVE` avoids reserving swap for the full writable mapping.

That tradeoff removes an up-front guarantee: exhausting RAM, swap, or commit while faulting a new CoW page can terminate the process. Applications must reclaim before reaching that point and should monitor both physical and commit pressure. Backing storage must also be large enough for every page eventually written to the real file.

## Stress comparison

The `stress` binary compares MooMap with a conventional `MAP_SHARED` mapping. By default it:

- preallocates a 20 GiB file;
- reads 1 TiB from `/dev/random`;
- chooses mildly biased random offsets;
- XORs each selected range with the random input;
- runs best-effort write-back every five seconds;
- forces reclaim of all dirty pages every 30 seconds, clearing the accumulated dirty set;
- checks physical-memory and commit pressure every 250 milliseconds and performs earlier reclaim toward 40% when either exceeds 80%;
- uses `MADV_WILLNEED` and `MADV_HUGEPAGE` for the standard mmap comparison.

The file is fully preallocated before mapping. This converts insufficient storage into a normal startup error instead of a later `SIGBUS`. Do not put a 20 GiB run in a small tmpfs such as `/tmp`; use a disk-backed filesystem with enough free space.

```bash
cargo run --release --bin stress -- \
  --mode moomap \
  --file ./moomap-stress.db \
  --file-size 20G \
  --io-bytes 1T \
  --chunk-size 64K
```

Then run the comparison against a separate file:

```bash
cargo run --release --bin stress -- \
  --mode mmap \
  --file ./mmap-stress.db \
  --file-size 20G \
  --io-bytes 1T \
  --chunk-size 64K
```

Accepted byte suffixes are `K`, `M`, `G`, and `T`. Run the benchmark in release mode; debug-mode loop overhead overwhelms the behavior being measured.

## Platform and toolchain

- Minimum supported Rust version: 1.85.0.
- The library uses Unix `mmap`, `pwrite`, and `madvise` interfaces.
- `MAP_NORESERVE`, transparent-hugepage advice, the stress runner's `/proc/meminfo` pressure tracking, and `/dev/random` workload target Linux.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
```

Every public API is documented. Changes should use Conventional Commit messages, remain independently buildable, and include focused behavioral coverage.

## License

Licensed under either the MIT License or the Apache License 2.0, at your option.
