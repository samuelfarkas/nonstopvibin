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
