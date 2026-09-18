<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->

# Mem Cache

I'm building an embedded database, designed to be highly concurrent. It works fine, but I'm currently having trouble with I/O bottlenecks. Even though I have plenty of available RAM, in UNIX-systems the kernel simply won't use that to cache my data, and keeps having major faults all the time. For bad disks this slows things down **a lot**. This repository is an experiment wrapper for `mmap` that should help caching more stuff and better using our RAM.

Come up with a funny name for it.

## how it works

We should  create a file-backed memory map, but use MAP_PRIVATE rather than MAP_SHARED. Every time a page gets dirtied, the kernel will CoW it and leave it as an anonymous page that can't be evicted like the shared one (it can go to swap, but not just be synced back to disk). We shold then have two methods:
 - Write Back: This will read the dirt pages and sync them back to the system. Notice, however that calling `msync` here won't do anything. You need to write that back into the real file. Take the pages, sort them by address and only try to batch long runs toguether to make it easier on spinning drives. Flusher shouldn't give the page back, we sync stuff to the file but keep all CoW available. This is 100% asyncronous and re-dirtying is totally fine! **Do not lock or add any mutual exclusion method here**, make this method 100% best-effort.
 - Reclaim: This should do what Write Back does, but also madvise WONT_NEED, so the kernel will take that page back. Once a page is marked for deletion, we need some write protectioin mechanism to avoid it being dirtied after write-back and before madise, come up with something **lightweight**, that doesn't stop all writers. Reading should not be disturbed.

Use hugepages to avoid too many translation entries and extreme fragmentation.

The strong version of write back (flush) is the consumer's responsibility, they should create a mutual exclusion access to the map and then call write back.


## Validation

Then you should write a stress test that will create a 20GB file, read 1TB from /dev/random and update the file by XOR-ing the current content with what it read. The exact position is also determined by /dev/random, but with a slight bias towards some positions. If the system memory usage gets close to 80%, we should call reclaim and bring that back to 40%. If usage is bellow that, we should just use available memory. Write Back should be called once every 5 seconds. Test this with a standard mmap with some madvise WILL_NEED LONG_LIVED to see if we made any difference.

