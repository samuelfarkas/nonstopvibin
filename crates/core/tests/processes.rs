//! Core processes with a fake core: the watchdog, the ledger and reaping, ported from
//! tests/core-orphans.test.ts where they need no pool.
//!
//! The watchdog guards only a process whose `ps` command line is exactly the one it was
//! spawned with, which a script run by an interpreter is not. So the fake core is this binary,
//! linked as `<root>/bin/cli-proxy-api` and run with `--config <path>`: it parks until
//! SIGTERM, or ignores SIGTERM when its config says `stubborn`. Run as `owner`, it is an app
//! instance that starts fake cores, records them and waits to be killed. Otherwise it runs
//! the tests, filtered by any name fragments given.
#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::indexing_slicing,
    reason = "a test binary without libtest: a panic is the failure report, stdout the log"
)]

use std::fs::{self, DirBuilder, OpenOptions};
use std::future::Future;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nsv_core::processes::{CoreProcess, Identify, KILL_GRACE, Ledger, Ps, forget, spawn_core};
use nsv_domain::id::ProfileId;
use nsv_store::layout::{
    core_arguments, core_config_path, core_ledger_directory, profile_directory,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--config") => fake_core(Path::new(&args[2])),
        Some("spawn") => spawn_one(
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
        ),
        Some("owner") => owner(
            Path::new(&args[2]),
            args[3].parse().unwrap(),
            Path::new(&args[4]),
        ),
        _ => run_tests(&args[1..]),
    }
}

// ---- The fake core and the owner instance ----

fn fake_core(config: &Path) -> ! {
    let text = fs::read_to_string(config).unwrap_or_default();
    if let Some(out) = text.strip_prefix("environment ") {
        let mut variables: Vec<String> = std::env::vars_os()
            .map(|(name, value)| format!("{}={}", name.display(), value.display()))
            .collect();
        variables.sort();
        fs::write(format!("{out}.part"), variables.join("\n")).unwrap();
        fs::rename(format!("{out}.part"), out).unwrap();
    }
    if text.contains("stubborn") {
        let runtime = runtime();
        runtime.block_on(async {
            let _ignored =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
            std::future::pending::<()>().await;
        });
    }
    loop {
        std::thread::park();
    }
}

/// A profile id per fake core.
fn profile(index: usize) -> ProfileId {
    ProfileId::parse(&format!("00000000-0000-4000-8000-{index:012}")).unwrap()
}

/// The profile directory and a config a fake core can read.
fn prepare(data: &Path, profile: &ProfileId, config: &str) {
    fs::create_dir_all(profile_directory(data, profile)).unwrap();
    fs::write(core_config_path(data, profile), config).unwrap();
}

/// Starts `count` fake cores, records each, prints `core <pid> <watchdog>` lines and
/// `self <pid> <identity>`, then waits to be killed with its cores still owned.
fn owner(data: &Path, count: usize, binary: &Path) -> ExitCode {
    runtime().block_on(async {
        let ledger = Ledger::new(data, Ps::default());
        let mut cores = Vec::new();
        for index in 0..count {
            let id = profile(index);
            prepare(data, &id, "fake");
            let mut core = spawn_core(binary, data, &id).unwrap();
            // ps can read a command line before exec settles; that read is not recorded.
            let started = Instant::now();
            while ledger.record(&mut core).await.unwrap().is_none() {
                assert!(started.elapsed() < Duration::from_secs(5), "never recorded");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            println!("core {} {}", core.pid(), core.watchdog().unwrap().unwrap());
            cores.push(core);
        }
        let me = std::process::id();
        println!(
            "self {me} {}",
            Ps::default().identify(me).await.unwrap().unwrap()
        );
        io::stdout().flush().unwrap();
        std::future::pending::<()>().await;
    });
    ExitCode::SUCCESS
}

/// Starts one fake core whose config names `out`, waits until it wrote its environment
/// there, and stops it.
fn spawn_one(data: &Path, binary: &Path, out: &Path) -> ExitCode {
    runtime().block_on(async {
        let mut core = spawn_core(binary, data, &profile(0)).unwrap();
        let started = Instant::now();
        while !out.exists() {
            assert!(started.elapsed() < Duration::from_secs(5), "no environment");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        core.terminate().await.unwrap();
    });
    ExitCode::SUCCESS
}

// ---- Helpers ----

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn run(test: impl Future<Output = ()>) {
    runtime().block_on(test);
}

struct Fixture {
    root: PathBuf,
    data: PathBuf,
    binary: PathBuf,
    tracked: Vec<Tracked>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    pid: u32,
    identity: String,
}

impl Fixture {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "nsv-processes-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let data = root.join("data");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&data)
            .unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        let binary = root.join("bin").join("cli-proxy-api");
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &binary).unwrap();
        Self {
            root,
            data,
            binary,
            tracked: Vec::new(),
        }
    }

    /// Tracks `pid` once `ps` shows its full command line.
    async fn track(&mut self, pid: u32) -> Tracked {
        let started = Instant::now();
        loop {
            let identity = identity(pid).await;
            if let Some(identity) = identity.filter(|i| !i.ends_with("(sh)")) {
                let tracked = Tracked { pid, identity };
                self.tracked.push(tracked.clone());
                return tracked;
            }
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "process {pid} has no readable command line"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn spawn(&self, index: usize, config: &str) -> CoreProcess {
        let id = profile(index);
        prepare(&self.data, &id, config);
        spawn_core(&self.binary, &self.data, &id).unwrap()
    }

    fn ledger_entries(&self) -> Vec<String> {
        ledger_entries(&self.data)
    }
}

impl Drop for Fixture {
    /// Signals only processes this test started that still carry the same identity.
    fn drop(&mut self) {
        for tracked in &self.tracked {
            let output = Command::new("ps")
                .args([
                    "-ww",
                    "-o",
                    "lstart=",
                    "-o",
                    "command=",
                    "-p",
                    &tracked.pid.to_string(),
                ])
                .env("LC_ALL", "C")
                .env("TZ", "UTC")
                .output()
                .unwrap();
            if String::from_utf8_lossy(&output.stdout).trim() == tracked.identity {
                let _ = Command::new("kill")
                    .args(["-KILL", &tracked.pid.to_string()])
                    .status();
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

async fn identity(pid: u32) -> Option<String> {
    Ps::default().identify(pid).await.unwrap()
}

async fn alive(process: &Tracked) -> bool {
    identity(process.pid).await.as_deref() == Some(process.identity.as_str())
}

/// Whether the process is gone within 10 s.
async fn gone(process: &Tracked) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if !alive(process).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

fn ledger_entries(data: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(core_ledger_directory(data)).map_or_else(
        |_| Vec::new(),
        |listing| {
            listing
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect()
        },
    );
    names.sort();
    names
}

fn write_entry(ledger: &Path, pid: u32, entry: &serde_json::Value) {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(ledger)
        .unwrap();
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(ledger.join(format!("{pid}.json")))
        .unwrap()
        .write_all(entry.to_string().as_bytes())
        .unwrap();
}

/// A synthetic identity: `lstart` width, then a command.
fn fake_identity(pid: u32) -> String {
    format!("Thu Jan  1 00:00:00 1970     process {pid}")
}

fn fixture_profile() -> ProfileId {
    profile(0)
}

fn core_identity(data: &Path) -> String {
    let arguments: Vec<String> = core_arguments(data, &fixture_profile())
        .iter()
        .map(|a| a.to_str().unwrap().to_owned())
        .collect();
    format!("Thu Jan  1 00:00:00 1970     core {}", arguments.join(" "))
}

/// A running owner instance: its process, its cores, their watchdogs and its own identity.
struct Instance {
    child: std::process::Child,
    cores: Vec<Tracked>,
    watchdogs: Vec<Tracked>,
    me: Tracked,
}

async fn start_owner(fixture: &mut Fixture, count: usize) -> Instance {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("owner")
        .arg(&fixture.data)
        .arg(count.to_string())
        .arg(&fixture.binary)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let (mut cores, mut watchdogs) = (Vec::new(), Vec::new());
    loop {
        let line = lines.next().expect("the owner reports").unwrap();
        let words: Vec<&str> = line.splitn(3, ' ').collect();
        match words[0] {
            "core" => {
                cores.push(fixture.track(words[1].parse().unwrap()).await);
                watchdogs.push(fixture.track(words[2].parse().unwrap()).await);
            }
            "self" => {
                let me = Tracked {
                    pid: words[1].parse().unwrap(),
                    identity: words[2].to_owned(),
                };
                fixture.tracked.push(me.clone());
                return Instance {
                    child,
                    cores,
                    watchdogs,
                    me,
                };
            }
            other => panic!("unexpected owner line {other}"),
        }
    }
}

fn kill(pid: u32) {
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
}

// ---- Tests ----

fn cores_exit_on_their_own_when_the_process_that_started_them_is_sigkilled() {
    run(async {
        let mut fixture = Fixture::new("sigkill");
        let mut instance = start_owner(&mut fixture, 2).await;
        assert_eq!(fixture.ledger_entries().len(), 2);
        instance.child.kill().unwrap();
        instance.child.wait().unwrap();
        for core in &instance.cores {
            assert!(
                gone(core).await,
                "an orphaned core is terminated by its watchdog"
            );
        }
        for watchdog in &instance.watchdogs {
            assert!(gone(watchdog).await, "the watchdog exits after its core");
        }
    });
}

fn a_new_instance_reaps_cores_of_a_dead_instance_and_refuses_reused_pids() {
    run(async {
        let mut fixture = Fixture::new("reap");
        let mut instance = start_owner(&mut fixture, 1).await;
        // Defeat layer 1 so the core really outlives its owner.
        kill(instance.watchdogs[0].pid);
        assert!(gone(&instance.watchdogs[0]).await);
        instance.child.kill().unwrap();
        instance.child.wait().unwrap();
        let orphan = instance.cores[0].clone();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            alive(&orphan).await,
            "without its watchdog the core survives"
        );
        // Entries from the dead owner whose PIDs now belong to unrelated processes.
        let mut bystanders = Vec::new();
        let mut sleepers = Vec::new();
        for _ in 0..2 {
            let sleeper = Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            bystanders.push(fixture.track(sleeper.id()).await);
            sleepers.push(sleeper);
        }
        let config = core_config_path(&fixture.data, &fixture_profile());
        let decoys = [
            // A core of this directory once had this PID; another process has it now.
            format!(
                "Thu Jan  1 00:00:00 1970     {} --config {}",
                fixture.binary.display(),
                config.display()
            ),
            // Exactly this process's identity, but it is not a core of this directory.
            bystanders[1].identity.clone(),
        ];
        let ledger = core_ledger_directory(&fixture.data);
        for (bystander, identity) in bystanders.iter().zip(&decoys) {
            write_entry(
                &ledger,
                bystander.pid,
                &serde_json::json!({
                    "pid": bystander.pid, "identity": identity,
                    "owner": { "pid": instance.me.pid, "identity": instance.me.identity },
                }),
            );
        }
        Ledger::new(&fixture.data, Ps::default())
            .reap_orphans()
            .await
            .unwrap();
        assert!(
            !alive(&orphan).await,
            "the orphan is stopped before reaping returns"
        );
        assert_eq!(
            fixture.ledger_entries(),
            Vec::<String>::new(),
            "every stale entry goes"
        );
        for bystander in &bystanders {
            assert!(alive(bystander).await, "a reused PID is never signalled");
        }
        for mut sleeper in sleepers {
            sleeper.kill().unwrap();
            sleeper.wait().unwrap();
        }
    });
}

fn reaping_never_touches_cores_of_a_live_instance_or_of_another_data_directory() {
    run(async {
        let mut fixture = Fixture::new("live");
        let other = fixture.root.join("other");
        let mut instance = start_owner(&mut fixture, 1).await;
        let core = instance.cores[0].clone();
        let entries = fixture.ledger_entries();
        Ledger::new(&fixture.data, Ps::default())
            .reap_orphans()
            .await
            .unwrap();
        assert!(alive(&core).await, "a live owner keeps its core");
        assert_eq!(fixture.ledger_entries(), entries);
        // Another directory's ledger naming this exact core, claimed by a dead owner.
        write_entry(
            &core_ledger_directory(&other),
            core.pid,
            &serde_json::json!({
                "pid": core.pid, "identity": core.identity,
                "owner": { "pid": 1, "identity": "Thu Jan  1 00:00:00 1970     gone" },
            }),
        );
        Ledger::new(&other, Ps::default())
            .reap_orphans()
            .await
            .unwrap();
        assert!(
            alive(&core).await,
            "a core of another data directory is left alone"
        );
        assert_eq!(ledger_entries(&other), Vec::<String>::new());
        instance.child.kill().unwrap();
        instance.child.wait().unwrap();
    });
}

fn a_failed_owner_identity_lookup_is_retried_by_the_next_record() {
    run(async {
        let fixture = Fixture::new("owner-retry");
        let mut core = fixture.spawn(0, "fake");
        let failures = AtomicUsize::new(1);
        let data = fixture.data.clone();
        let identify = |pid: u32| {
            let answer = if pid != std::process::id() {
                Ok(Some(core_identity(&data)))
            } else if failures
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                .is_ok()
            {
                Err(io::Error::other("spawn ps EMFILE"))
            } else {
                Ok(Some(fake_identity(pid)))
            };
            std::future::ready(answer)
        };
        let ledger = Ledger::new(&fixture.data, identify);
        assert!(ledger.record(&mut core).await.is_err());
        let path = ledger.record(&mut core).await.unwrap().unwrap();
        assert_eq!(
            path,
            core_ledger_directory(&fixture.data).join(format!("{}.json", core.pid()))
        );
        assert_eq!(fixture.ledger_entries(), [format!("{}.json", core.pid())]);
        core.terminate().await.unwrap();
    });
}

fn an_owner_identity_lookup_without_ps_writes_nothing_and_is_retried() {
    run(async {
        let fixture = Fixture::new("no-ps");
        let mut core = fixture.spawn(0, "fake");
        let unavailable = Ps::new(fixture.root.join("missing-ps"));
        let calls = AtomicUsize::new(0);
        let data = fixture.data.clone();
        let identify = |pid: u32| {
            let first_owner =
                pid == std::process::id() && calls.fetch_add(1, Ordering::Relaxed) == 0;
            let data = data.clone();
            let unavailable = unavailable.clone();
            async move {
                if pid != std::process::id() {
                    Ok(Some(core_identity(&data)))
                } else if first_owner {
                    unavailable.identify(pid).await
                } else {
                    Ps::default().identify(pid).await
                }
            }
        };
        let ledger = Ledger::new(&fixture.data, identify);
        let error = ledger.record(&mut core).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            !core_ledger_directory(&fixture.data).exists(),
            "no entry for that core"
        );
        assert!(ledger.record(&mut core).await.unwrap().is_some());
        assert_eq!(
            calls.load(Ordering::Relaxed),
            2,
            "the owner was looked up again"
        );
        core.terminate().await.unwrap();
    });
}

fn a_core_identity_read_before_its_exec_settles_is_not_recorded() {
    run(async {
        let fixture = Fixture::new("settle");
        let mut core = fixture.spawn(0, "fake");
        let settled = AtomicUsize::new(0);
        let data = fixture.data.clone();
        let identify = |pid: u32| {
            std::future::ready(Ok(Some(if pid == std::process::id() {
                fake_identity(pid)
            } else if settled.load(Ordering::Relaxed) == 1 {
                core_identity(&data)
            } else {
                // Until exec settles, ps can show the parent's command line (Bun under qemu-user).
                "Thu Jan  1 00:00:00 1970     bun server.ts".to_owned()
            })))
        };
        let ledger = Ledger::new(&fixture.data, identify);
        assert_eq!(ledger.record(&mut core).await.unwrap(), None);
        assert!(
            !core_ledger_directory(&fixture.data).exists(),
            "no entry that could never be reaped"
        );
        settled.store(1, Ordering::Relaxed);
        assert!(ledger.record(&mut core).await.unwrap().is_some());
        core.terminate().await.unwrap();
    });
}

fn reaping_without_ps_keeps_the_entry_and_signals_nothing() {
    run(async {
        let mut fixture = Fixture::new("reap-no-ps");
        // A shell blocked in its builtin read, with the command line of a core of this directory.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "read _", "core"])
            .args(core_arguments(&fixture.data, &fixture_profile()))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let core = fixture.track(child.id()).await;
        let ledger = core_ledger_directory(&fixture.data);
        let me = std::process::id();
        write_entry(
            &ledger,
            core.pid,
            &serde_json::json!({
                "pid": core.pid, "identity": core.identity,
                "owner": { "pid": me, "identity": fake_identity(me) },
            }),
        );
        let name = format!("{}.json", core.pid);
        Ledger::new(&fixture.data, Ps::new(fixture.root.join("missing-ps")))
            .reap_orphans()
            .await
            .unwrap();
        assert_eq!(
            fixture.ledger_entries(),
            std::slice::from_ref(&name),
            "the entry stays"
        );
        assert!(alive(&core).await, "nothing is signalled");
        // Control: with ps, the same entry is an orphan of a dead owner and is reaped.
        Ledger::new(&fixture.data, Ps::default())
            .reap_orphans()
            .await
            .unwrap();
        assert!(!alive(&core).await);
        assert_eq!(fixture.ledger_entries(), Vec::<String>::new());
        child.wait().unwrap();
    });
}

fn a_normal_stop_leaves_no_watchdog_core_or_ledger_entry() {
    run(async {
        let mut fixture = Fixture::new("stop");
        let mut core = fixture.spawn(0, "fake");
        let ledger = Ledger::new(&fixture.data, Ps::default());
        let path = loop {
            if let Some(path) = ledger.record(&mut core).await.unwrap() {
                break path;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let tracked_core = fixture.track(core.pid()).await;
        let watchdog = fixture.track(core.watchdog().unwrap().unwrap()).await;
        let status = core.terminate().await.unwrap();
        assert_eq!(status.signal(), Some(15), "SIGTERM was enough");
        forget(&path).unwrap();
        assert!(!alive(&tracked_core).await);
        assert!(gone(&watchdog).await);
        assert_eq!(fixture.ledger_entries(), Vec::<String>::new());
        forget(&path).unwrap();
    });
}

fn a_core_that_ignores_sigterm_is_killed_after_the_grace() {
    run(async {
        let mut fixture = Fixture::new("stubborn");
        let mut core = fixture.spawn(0, "stubborn");
        fixture.track(core.pid()).await;
        // Let the fake core install its handler.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        let status = core.terminate().await.unwrap();
        assert_eq!(status.signal(), Some(9));
        assert!(started.elapsed() >= KILL_GRACE, "{:?}", started.elapsed());
    });
}

fn dropping_a_core_closes_the_watchdog_pipe_and_the_watchdog_stops_it() {
    run(async {
        let mut fixture = Fixture::new("drop");
        let core = fixture.spawn(0, "fake");
        let tracked = fixture.track(core.pid()).await;
        let watchdog = fixture.track(core.watchdog().unwrap().unwrap()).await;
        drop(core);
        assert!(gone(&tracked).await, "the watchdog stopped the core");
        assert!(gone(&watchdog).await);
    });
}

fn a_core_inherits_only_the_allowed_environment() {
    let fixture = Fixture::new("environment");
    let out = fixture.root.join("environment");
    prepare(
        &fixture.data,
        &profile(0),
        &format!("environment {}", out.display()),
    );
    // An instance whose own environment holds keys, proxies and provider settings.
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("spawn")
        .arg(&fixture.data)
        .arg(&fixture.binary)
        .arg(&out)
        .env_clear()
        .envs([
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/home/fixture"),
            ("TMPDIR", "/tmp/fixture"),
            ("LANG", "C.UTF-8"),
            ("LC_ALL", ""),
            ("SystemRoot", "C:\\Windows"),
            ("OPENAI_API_KEY", "sk-fixture"),
            ("ANTHROPIC_BASE_URL", "https://proxy.example"),
            ("HTTPS_PROXY", "http://proxy.example:8080"),
            ("http_proxy", "http://proxy.example:8080"),
            ("REDIS_URL", "redis://cache.example"),
            ("NONSTOPVIBIN_DATA_DIR", "/elsewhere"),
            ("TZ", "UTC"),
        ])
        .status()
        .unwrap();
    assert!(status.success());
    // An empty variable is not passed on.
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "HOME=/home/fixture\nLANG=C.UTF-8\nPATH=/usr/bin:/bin\nSystemRoot=C:\\Windows\nTMPDIR=/tmp/fixture"
    );
}

/// Context switches of `pid` so far, from /proc.
#[cfg(target_os = "linux")]
fn switches(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .filter(|line| line.contains("ctxt_switches"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn the_watchdog_never_wakes_while_its_core_runs() {
    #[cfg(target_os = "linux")]
    run(async {
        let mut fixture = Fixture::new("idle");
        let mut core = fixture.spawn(0, "fake");
        let watchdog = fixture.track(core.watchdog().unwrap().unwrap()).await;
        // Past its identity read, it blocks in read.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let before = switches(watchdog.pid);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(
            switches(watchdog.pid),
            before,
            "the watchdog woke while idle"
        );
        core.terminate().await.unwrap();
    });
}

fn run_tests(arguments: &[String]) -> ExitCode {
    let tests: [(&str, fn()); 12] = [
        (
            "a_core_inherits_only_the_allowed_environment",
            a_core_inherits_only_the_allowed_environment,
        ),
        (
            "cores_exit_on_their_own_when_the_process_that_started_them_is_sigkilled",
            cores_exit_on_their_own_when_the_process_that_started_them_is_sigkilled,
        ),
        (
            "a_new_instance_reaps_cores_of_a_dead_instance_and_refuses_reused_pids",
            a_new_instance_reaps_cores_of_a_dead_instance_and_refuses_reused_pids,
        ),
        (
            "reaping_never_touches_cores_of_a_live_instance_or_of_another_data_directory",
            reaping_never_touches_cores_of_a_live_instance_or_of_another_data_directory,
        ),
        (
            "a_failed_owner_identity_lookup_is_retried_by_the_next_record",
            a_failed_owner_identity_lookup_is_retried_by_the_next_record,
        ),
        (
            "an_owner_identity_lookup_without_ps_writes_nothing_and_is_retried",
            an_owner_identity_lookup_without_ps_writes_nothing_and_is_retried,
        ),
        (
            "a_core_identity_read_before_its_exec_settles_is_not_recorded",
            a_core_identity_read_before_its_exec_settles_is_not_recorded,
        ),
        (
            "reaping_without_ps_keeps_the_entry_and_signals_nothing",
            reaping_without_ps_keeps_the_entry_and_signals_nothing,
        ),
        (
            "a_normal_stop_leaves_no_watchdog_core_or_ledger_entry",
            a_normal_stop_leaves_no_watchdog_core_or_ledger_entry,
        ),
        (
            "a_core_that_ignores_sigterm_is_killed_after_the_grace",
            a_core_that_ignores_sigterm_is_killed_after_the_grace,
        ),
        (
            "dropping_a_core_closes_the_watchdog_pipe_and_the_watchdog_stops_it",
            dropping_a_core_closes_the_watchdog_pipe_and_the_watchdog_stops_it,
        ),
        (
            "the_watchdog_never_wakes_while_its_core_runs",
            the_watchdog_never_wakes_while_its_core_runs,
        ),
    ];
    // cargo passes libtest flags and name filters to every test binary.
    let filters: Vec<&String> = arguments.iter().filter(|a| !a.starts_with('-')).collect();
    let mut passed = 0;
    for (name, test) in tests {
        if filters.is_empty() || filters.iter().any(|filter| name.contains(filter.as_str())) {
            println!("test {name} ...");
            test();
            println!("test {name} ... ok");
            passed += 1;
        }
    }
    println!("test result: ok. {passed} passed");
    ExitCode::SUCCESS
}
