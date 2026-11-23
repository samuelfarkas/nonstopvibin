//! Measurements behind docs/decisions/rust-port.md, run on demand under an X server:
//!
//! ```sh
//! cargo test --release -p nsv-window --test integration -- --ignored --nocapture measure::
//! ```
//!
//! The filter keeps it alone in its process: its in-process window starts GTK on its test
//! thread, and GTK stays bound to the first thread that starts it (`helper.rs`'s clipboard
//! check starts it on another).
//!
//! Latency: from asking for a window to its page having loaded and the window showing, for
//! the helper (spawn to `Ready`) and for wry inside an already running process, interleaved.
//! Memory: the helper's process tree PSS with the main window and with the quota popup open.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nsv_desktop::pipe::{FromHelper, Rect, ToHelper, View};
use nsv_domain::endpoint::Origin;
use nsv_domain::id::ProfileId;
use tao::event::Event;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::{EventLoopBuilderExtUnix, WindowExtUnix};
use tao::window::WindowBuilder;
use wry::{PageLoadEvent, WebViewBuilder, WebViewBuilderExtUnix};

use crate::common::{Helper, Server, pause, tree};

const PAIRS: usize = 12;
/// Lets each window's WebKit processes exit before the next one starts.
const SETTLE: Duration = Duration::from_secs(1);

fn binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_nonstopvibin-window"))
}

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("nsv-measure-{name}-{}", std::process::id()));
    fs::create_dir_all(&path).unwrap();
    path
}

fn quota() -> View {
    View::Quota {
        anchor: Rect {
            x: 1200,
            y: 0,
            width: 24,
            height: 24,
        },
    }
}

/// Spawn to `Ready`: the page has loaded and the window shows.
fn helper_open(origin: &Origin, data: &Path) -> Duration {
    let started = Instant::now();
    let mut helper = Helper::start(binary(), origin, quota(), data);
    assert_eq!(helper.next(), FromHelper::Ready);
    let elapsed = started.elapsed();
    helper.send(&ToHelper::Quit);
    assert!(helper.wait().success());
    elapsed
}

enum Step {
    Open,
    Loaded,
}

/// The same window in this process: build to page loaded and shown, on a warm GTK loop,
/// as a daemon that links WebKit would open it. Helper runs interleave with these.
fn interleaved(origin: &Origin, data: &Path) -> (Vec<Duration>, Vec<Duration>) {
    let mut event_loop = EventLoopBuilder::<Step>::with_user_event()
        .with_any_thread(true)
        .build();
    let proxy = event_loop.create_proxy();
    let url = format!("{}/?view=tray", origin.as_str());
    let (mut in_process, mut spawned) = (Vec::new(), Vec::new());
    let mut open = None;
    let mut started = Instant::now();
    proxy.send_event(Step::Open).unwrap_or_default();
    event_loop.run_return(|event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(Step::Open) => {
                started = Instant::now();
                let window = WindowBuilder::new()
                    .with_inner_size(tao::dpi::LogicalSize::new(386, 640))
                    .with_visible(false)
                    .build(target)
                    .unwrap();
                let loaded = proxy.clone();
                let webview = WebViewBuilder::new()
                    .with_url(&url)
                    .with_on_page_load_handler(move |event, _| {
                        if matches!(event, PageLoadEvent::Finished) {
                            loaded.send_event(Step::Loaded).unwrap_or_default();
                        }
                    })
                    .build_gtk(window.default_vbox().unwrap())
                    .unwrap();
                open = Some((webview, window));
            }
            Event::UserEvent(Step::Loaded) => {
                if let Some((_, window)) = &open {
                    window.set_visible(true);
                }
                in_process.push(started.elapsed());
                open = None;
                pause(SETTLE);
                spawned.push(helper_open(origin, data));
                pause(SETTLE);
                if in_process.len() == PAIRS {
                    *control_flow = ControlFlow::Exit;
                } else {
                    proxy.send_event(Step::Open).unwrap_or_default();
                }
            }
            _ => {}
        }
    });
    (in_process, spawned)
}

fn millis(samples: &[Duration]) -> Vec<f64> {
    let mut millis: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    millis.sort_by(f64::total_cmp);
    millis
}

fn median(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        f64::midpoint(sorted[middle - 1], sorted[middle])
    } else {
        sorted[middle]
    }
}

/// The helper tree's PSS once `step` has been reported and the page has settled.
fn helper_pss(server: &mut Server, view: View, step: &str, data: &Path) -> (f64, usize) {
    let mut helper = Helper::start(binary(), &server.origin(), view, data);
    assert_eq!(helper.next(), FromHelper::Ready);
    server.result(step);
    pause(Duration::from_secs(3));
    let pids = tree(helper.pid());
    let pss = pss_mib(&pids);
    helper.send(&ToHelper::Quit);
    assert!(helper.wait().success());
    pause(SETTLE);
    (pss, pids.len())
}

/// Proportional set size of `pids` in MiB, from `smaps_rollup`.
fn pss_mib(pids: &HashSet<u32>) -> f64 {
    let kib: u64 = pids
        .iter()
        .filter_map(|pid| fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok())
        .filter_map(|rollup| {
            rollup.lines().find_map(|line| {
                let value = line.strip_prefix("Pss:")?.trim().trim_end_matches(" kB");
                value.parse::<u64>().ok()
            })
        })
        .sum();
    #[expect(clippy::cast_precision_loss, reason = "a few GiB at most")]
    let mib = kib as f64 / 1024.0;
    mib
}

#[test]
#[ignore = "a measurement, run on demand"]
#[expect(clippy::print_stdout, reason = "the measurement's report")]
fn window_latency_and_memory() {
    let mut server = Server::start();
    let data = directory("latency");
    let origin = server.origin();
    // One unmeasured open of each, so neither pays for first-time disk reads.
    helper_open(&origin, &data);
    let (in_process, spawned) = interleaved(&origin, &data);
    let (in_process, spawned) = (millis(&in_process), millis(&spawned));
    let gap = median(&spawned) - median(&in_process);
    println!(
        "in-process wry, ms (sorted, n={}): {in_process:.1?}",
        in_process.len()
    );
    println!(
        "helper process, ms (sorted, n={}): {spawned:.1?}",
        spawned.len()
    );
    println!(
        "median: in-process {:.1} ms, helper {:.1} ms, helper slower by {gap:.1} ms",
        median(&in_process),
        median(&spawned)
    );

    let profile = ProfileId::parse("1c2d3e4f-5a6b-4c7d-8e9f-0a1b2c3d4e5f").unwrap();
    for run in 1..=3 {
        let main = View::Main {
            profile: Some(profile.clone()),
        };
        let (main_pss, main_count) = helper_pss(&mut server, main, "guarded", &directory("pss"));
        let (quota_pss, quota_count) = helper_pss(&mut server, quota(), "tray", &directory("pss"));
        println!(
            "run {run}: PSS main window {main_pss:.1} MiB ({main_count} processes), \
             quota popup {quota_pss:.1} MiB ({quota_count} processes)"
        );
    }
}
