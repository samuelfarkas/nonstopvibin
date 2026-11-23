# Evidence for the Rust port decisions

Measurements behind [the decision record](rust-port.md). All on Hive (Ryzen 5 3600, Linux 6.12,
a shared host), with load recorded beside each run.

## Shell prototypes (Linux, Docker with Xvfb)

Four throwaway designs. Each serves a page over loopback with tokio and hyper and opens it in a
webview on demand. The figures cover the whole process tree, WebKit processes included. Each
was run 3 times, interleaved, at load 0.3–1.7. WebKitGTK 2.54.3, wry 0.57.3, tao 0.37.3,
Tauri 2.12.7, ksni 0.3.7.

States: (a) started, no window ever opened; (b) after three open/close cycles and 10 s settle;
(c) window open.

| Design                                        | State | PSS MiB (median, range) | Wakeups/s     | Processes                        |
| --------------------------------------------- | ----- | ----------------------- | ------------- | -------------------------------- |
| P0: hyper only, no GUI crates                 | a     | 1.0 (1.0–1.1)           | 0.0           | 1                                |
| P0 linked to libwebkit2gtk, never initialised | a     | 31.1 (30.9–31.1)        | 0.0           | 1                                |
| P1: Tauri 2                                   | a     | 80.0 (79.5–80.8)        | 0.2           | 1                                |
| P1: Tauri 2                                   | b     | 179.4 (178.4–180.2)     | 0.4           | 2 (a WebKitNetworkProcess stays) |
| P1: Tauri 2                                   | c     | 323.6 (323.3–325.5)     | 4.3 (4.3–5.0) | 3                                |
| P2: wry + tao in the daemon                   | a     | 71.9 (70.8–72.5)        | 0.2           | 1                                |
| P2: wry + tao in the daemon                   | b     | 141.1 (140.8–141.5)     | 0.4           | 1                                |
| P2: wry + tao in the daemon                   | c     | 315.3 (314.0–316.2)     | 0.4           | 3                                |
| **P3: GUI-free daemon + window process**      | a     | **2.8 (2.7–2.9)**       | **0.0**       | 1                                |
| **P3: GUI-free daemon + window process**      | b     | **2.8 (2.7–2.9)**       | **0.0**       | 1                                |
| P3: GUI-free daemon + window process          | c     | 313.7 (313.0–313.8)     | 0.5           | 4                                |

- Linking WebKitGTK costs about 30 MiB even when it is never called, so a daemon that can open
  a window in-process starts above the 15–20 MiB target.
- With a window open, every design costs about 314–324 MiB. The choice only decides what is left
  when the window closes, and only P3 returns to its starting footprint.
- Inside the container, PSS charges the GTK libraries in full, so P1 and P2 would read lower
  on a desktop that shares those libraries. The ranking stays the same.
- Not measured here: how long a window takes to open (P3 starts a process per open), a real tray
  host, Wayland, and macOS.

## The window helper (Linux, Docker with Xvfb)

`nonstopvibin-window` at its first commit, release build, WebKitGTK 2.54.3, measured by
`crates/window/tests/measure.rs` (`cargo test --release -p nsv-window --test measure -- --ignored
--nocapture`) at load 1.5–2.2. The page is the test harness page, not the React UI.

| Measure                                                   | Median    | Range           | Runs |
| --------------------------------------------------------- | --------- | --------------- | ---- |
| Open to page loaded and shown, `wry` in a running process | 136.9 ms  | 131.7–216.5 ms  | 12   |
| Spawn to `Ready` (page loaded and shown), helper process  | 266.5 ms  | 257.2–294.3 ms  | 12   |
| Helper PSS, main window open (3 processes)                | 269.0 MiB | 266.3–273.1 MiB | 3    |
| Helper PSS, quota popup open (3 processes)                | 237.5 MiB | 235.8–237.8 MiB | 3    |

- The two latency series ran interleaved, after one unmeasured open of each. The helper is
  129.6 ms slower, under the 150 ms that would keep it alive after the last close.
- Where the extra time goes: the dynamic loader takes about 3 ms (`LD_DEBUG=statistics`), and
  spawn to `Ready` is the same 260–285 ms when the page cannot load at all. The rest is GTK and
  WebKit starting in a fresh process; it was not broken down further.
- Not measured: macOS, a desktop session with shared libraries, and the real UI bundle.

## The desktop shell idle (Linux, Docker)

`nsv-shell` alone (`crates/shell/examples/idle.rs`, release build): its tray registered with a
StatusNotifierWatcher on a session bus, no window ever opened, measured for 60 s after a 5 s
settle by `crates/shell/tests/desktop.rs` (`measure_idle`). Three runs at load 0.4–2.7.

| Measure                                | Value (3 runs)      |
| -------------------------------------- | ------------------- |
| PSS                                    | 3.5 MiB (3.48–3.51) |
| Threads                                | 1                   |
| Context switches in 60 s (all threads) | 0, 0, 0             |

- The tray and the supervisor add no wakeups. One thread runs tokio's current-thread runtime and
  ksni's D-Bus connection, and with no timer set it does not wake. A tray host that queries the
  item wakes it; the test's watcher does not.
- The figure excludes the server, which the daemon adds, and a real tray host.

## Main (4ea188a) on the before/after harness

Three API-key profiles on a synthetic upstream. Streamed events are paced 5 ms apart. Two runs
per host; ranges cover both runs.

| Metric                                 | Bun 1.4.2 server               | Electron 44, no window               |
| -------------------------------------- | ------------------------------ | ------------------------------------ |
| Startup to TCP accept                  | 48–101 ms                      | 164–995 ms                           |
| Idle host PSS                          | 43.5–43.7 MiB                  | 178.6–185.1 MiB (7 processes)        |
| Idle host wakeups/s                    | 2.1–3.2                        | 2.0–2.6                              |
| Host wakeups/s 120–180 s after a burst | 21.9–51.6                      | 1.9–2.0                              |
| Host CPU, 200-event stream             | 18.5–47.5 ms (93–238 µs/event) | 23–35 ms (115–175 µs/event)          |
| Host CPU, 1 MB request                 | 4.3–6.0 ms                     | 3.3–5.7 ms                           |
| Host RSS right after 1 MB × 8 burst    | 90–101 MiB                     | 125 MiB (main process)               |
| Install size                           | 78 MB binary                   | 362 MB app (Electron runtime 283 MB) |

The first Bun run overlapped another workload: its stream numbers are about twice the second
run's. CPU per request comes from `/proc/<pid>/stat` ticks over 20–60 requests.

## Linux allocator, interleaved A/B (caeab18)

Release builds on the before/after harness, five daemons each with its own data dir and its own
copy of the binary. Ten rounds in rotated order, n = 10 per cell, every request answered 200.
Host load averages ranged from 0.7 to 6.6. Host CPU is schedstat time of the daemon per request.
Cells show median and range.

| Metric                     | glibc (cannot ship)  | static musl, mallocng | musl + mimalloc 3.3  | musl + mimalloc, THP off |
| -------------------------- | -------------------- | --------------------- | -------------------- | ------------------------ |
| Host CPU, 290 KB request   | 0.33 (0.32–0.37) ms  | 0.56 (0.52–0.73) ms   | 0.35 (0.31–0.43) ms  | 0.37 (0.33–0.46) ms      |
| Host CPU, 1 MB request     | 0.80 (0.77–0.84) ms  | 1.06 (1.01–1.48) ms   | 0.81 (0.77–0.89) ms  | 0.86 (0.80–1.04) ms      |
| Host CPU, 200-event stream | 7.47 (6.44–12.05) ms | 7.48 (6.99–9.12) ms   | 7.39 (6.53–14.64) ms | 7.68 (6.86–8.47) ms      |
| Minor faults, 1 MB request | 2                    | 87                    | 0                    | 2                        |
| p50 latency, 1 MB request  | 16.5 ms              | 16.8 ms               | 16.3 ms              | 17.1 ms                  |
| Idle PSS                   | 9.1 MiB              | 10.8 MiB              | 18.9 MiB             | 9.7 MiB                  |
| PSS right after 1 MB × 8   | 12.5 MiB             | 11.4 MiB              | 34.4 MiB             | 21.9 MiB                 |
| PSS over eight such bursts | not run              | 11.1–11.9 MiB         | 30.9–40.3 MiB        | 15.5–30.9 MiB            |

- Decision: musl's own allocator. Its extra CPU is page faults. mallocng serves large buffers
  from fresh mmap pages and gives freed ones back, so a 1 MB request takes 87 minor faults
  against 0–2 for the others, and system time doubles. The gap is 0.2–0.26 ms of a request that
  costs 7–22 ms of core CPU. p50 latency and the paced stream show no difference.
- mimalloc's memory cost has two causes. Its eagerly committed arenas get huge pages under
  THP `always` (a build-time define turns that off), and it purges freed memory only on later
  allocator activity after a 1 s delay, which an idle daemon never triggers. That delay can be
  set only at run time, through the environment or unsafe code.
- Memory stays bounded for every variant. The interleaved stream also shows that the earlier
  musl stream figure (9.8–14.3 ms) came from host load, not the allocator.
