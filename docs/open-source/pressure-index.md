# Pressure index

Each OS reports memory pressure differently. canfit translates its native signals into one shared
index so the answer means the same thing everywhere:

- a **score from 0 to 100**;
- a **level**: ● **calm** (0–39) · ▲ **strained** (40–69) · ✗ **critical** (70–100). Colour is
  always paired with a symbol;
- a **main reason** in one sentence, for example "swap is growing (8.0 MB/s)", "macOS is
  compressing memory to make room (4.3 GB of compressed memory)" or "Windows has promised 29.0 GB
  of the 32.0 GB it can give out (RAM plus page file)" or "programs spent 35% of the last 10
  seconds waiting for memory" (Linux).

The score is always labelled **estimated**: it is a heuristic built on measured signals. The
thresholds below are a first calibration on a 16 GB Apple Silicon Mac (the Windows-only and
Linux-only signals are not calibrated yet) and will change as more real measurements come in.

## How the score is built

Every signal gets its own score from 0 to 100. The overall score is the **highest** signal score,
so the main reason always names the signal that drives the level. When several signals tie, the
most concrete one wins (growing swap, then low available memory, then commit charge, then memory
stalls, then hard page faults, then compression, then the OS's own level). When the OS's own
level is the main reason, canfit adds the strongest concrete signal behind it ("macOS reports memory pressure as warning; only 1.6 GB of 16.0 GB memory is
available"). Signals scoring under 20 are not named; the reason then simply says how much memory
is available.

| Signal | Scores 0 when | Rises to | Notes |
|---|---|---|---|
| OS pressure level | the OS reports *normal* | 60 at *warning*, 90 at *critical* | macOS `kern.memorystatus_vm_pressure_level`; Windows low-memory notification (critical only) |
| Available memory | 25 % or more of memory is available | 90 at 5 % or less, linear in between | cache the OS drops on its own counts as available |
| Compression | the compressor holds 10 % of memory or less | 65 at 35 % or more, linear in between | capped below *critical*: compression is the OS coping, not failing |
| Swap growth | swap grows slower than 1 MB/s | 50 at 1 MB/s, 85 at 32 MB/s or more | swap that is merely in use scores 0 |
| Commit charge (Windows) | 80 % or less of the commit limit is committed | 90 at 97 % or more, linear in between | at the limit Windows refuses new allocations |
| Hard page faults (Windows) | 500 pages/s or fewer are read back from disk | 65 at 5,000 pages/s or more, linear in between | capped below *critical*: it also counts apps reading their own files when they start |
| Memory stalls (Linux PSI) | programs waited for memory 10 % or less of the last 10 s (`some avg10`) and all programs at once 2 % or less (`full avg10`) | `some`: 75 at 50 %; `full`: 95 at 20 %; linear in between, the higher of the two | `full` means the machine did nothing but wait for memory; 20 % is far below systemd-oomd's default 60 % kill limit |

## Rules

- **Swap is a warning only when it is growing.** Old, idle swap is harmless and does not raise the
  level. Growth is the faster of the change in used swap and the rate of data written to swap,
  measured over the sampler's window (0.25 s for the one-shot `canfit`, longer in the live
  monitor).
- **Cache is not "used".** Memory the system frees on its own counts as available.
- The summary names the biggest application when memory is under pressure, for example "Memory
  under pressure: swap is growing (8.0 MB/s). Google Chrome is using 9.2 GB."

## Signals per OS

### macOS

| Signal | Source |
|---|---|
| Memory used / available | `host_statistics64`, with Activity Monitor's formulas: *Memory Used* = App Memory + Wired + Compressed; App Memory = anonymous − purgeable pages; available = total − used |
| Compressed memory | physical pages occupied by the compressor (Activity Monitor's *Compressed*) |
| Swap and its growth | `sysctl vm.swapusage` and the cumulative swap-out counter |
| OS pressure level | `sysctl kern.memorystatus_vm_pressure_level` (1 normal, 2 warning, 4 critical) |

No admin rights are needed.

### Windows

| Signal | Source |
|---|---|
| Memory used / available | `GlobalMemoryStatusEx`: available = free, zeroed and standby memory (Task Manager's *Available*); used = total − available |
| Commit charge and limit | `GetPerformanceInfo` (Task Manager's *Committed*) |
| Swap and its growth | the page files (`EnumPageFilesW`); growth is the change in page-file use |
| Hard page faults | PDH `\Memory\Pages Input/sec`, averaged since the previous reading |
| Compressed memory | the `Memory Compression` process's working set, only when Windows lets canfit read it (usually as administrator) |
| OS pressure level | the low-memory resource notification (`QueryMemoryResourceNotification`): *critical* when set |

No admin rights are needed. PDH counters are read by their English names, so they work in every
display language.

### Linux

| Signal | Source |
|---|---|
| Memory used / available | `/proc/meminfo`: available = `MemAvailable` (the kernel's own estimate of memory a new program gets without swapping; on kernels before 3.14 canfit estimates it from free memory and reclaimable cache and labels it *estimated*); used = total − available. `free -b`'s *used* column is lower: it leaves out shared memory and unreclaimable caches |
| Cached files | `Buffers + Cached + SReclaimable` (`free`'s *buff/cache*) |
| Compressed memory | zswap (`Zswap`/`Zswapped` in `/proc/meminfo`, Linux 5.19+) plus zram devices (`/sys/block/zram*/mm_stat`) |
| Swap and its growth | `SwapTotal`/`SwapFree` and `/proc/vmstat`'s cumulative `pswpout` |
| Memory stalls | PSI `/proc/pressure/memory` (Linux 4.20+); when it is missing, the cgroup v2 root's `/sys/fs/cgroup/memory.pressure`; CPU and I/O stalls are read and shown but do not raise the memory level |
| OS pressure level | none: Linux has no verdict of its own; PSI is its own signal |

No root is needed. Without PSI (an older kernel, or PSI turned off with `psi=0`) the level comes
from available memory, compression and swap growth alone.
