use std::fs;
use std::path::Path;
use std::process::Command;

use nsv_core::CoreError;
use nsv_core::binary::{CoreTarget, Version, bundled_pin};
use nsv_core::install::{
    ActiveCore, Staging, install_archive, install_core, prune, select_installed_core,
};
use nsv_testkit::TempDir;

use crate::common::{CORE_SHA256, LINUX_X64, digest, mode, pin};

/// A gzipped tar holding `files` at its top level, as upstream releases are.
fn archive(directory: &Path, files: &[(&str, &str)]) -> std::path::PathBuf {
    let contents = directory.join("contents");
    fs::create_dir_all(&contents).unwrap();
    for (name, body) in files {
        fs::write(contents.join(name), body).unwrap();
    }
    let path = directory.join("release.tar.gz");
    let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&path)
        .arg("-C")
        .arg(&contents)
        .args(&names)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

fn app_error(result: Result<(), CoreError>) -> (String, u16) {
    match result {
        Err(CoreError::App(error)) => (error.message, error.status),
        other => panic!("expected an AppError, got {other:?}"),
    }
}

const MISMATCH: &str = "The downloaded core does not match the reviewed pin. Nothing was changed.";

#[tokio::test]
async fn an_archive_matching_the_pin_installs_its_executable_license_and_manifest() {
    let root = TempDir::new("install");
    let release = archive(
        root.path(),
        &[
            ("cli-proxy-api", "core"),
            ("LICENSE", "MIT"),
            ("README.md", "x"),
        ],
    );
    let sha256 = digest(&release);
    let pin = pin("8.1.0", &sha256.to_string(), CORE_SHA256);
    let target = root.path().join("core");
    fs::create_dir(&target).unwrap();
    install_archive(&pin, LINUX_X64, &release, sha256, &target, true)
        .await
        .unwrap();
    let mut names: Vec<String> = fs::read_dir(&target)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["LICENSE", "cli-proxy-api", "manifest.json"]);
    assert_eq!(
        fs::read_to_string(target.join("cli-proxy-api")).unwrap(),
        "core"
    );
    assert_eq!(mode(&target.join("cli-proxy-api")), 0o755);
    assert_eq!(mode(&target.join("LICENSE")), 0o644);
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(target.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest,
        serde_json::json!({
            "version": "8.1.0", "platform": "linux", "arch": "x64",
            "archive": "CLIProxyAPI_8.1.0_linux_amd64.tar.gz",
            "sha256": sha256.to_string(), "binarySha256": CORE_SHA256,
        })
    );
    nsv_core::verify_core(&target.join("cli-proxy-api"), Some(&pin)).unwrap_or_else(|error| {
        // verify_core pins the running platform; on other targets only the manifest matters.
        assert!(CoreTarget::current().is_err(), "{error}");
    });
}

#[tokio::test]
async fn an_archive_or_executable_off_the_pin_is_refused() {
    let root = TempDir::new("refuse");
    let release = archive(root.path(), &[("cli-proxy-api", "tampered")]);
    let sha256 = digest(&release);
    for (pin, label) in [
        (pin("8.1.0", &"a".repeat(64), CORE_SHA256), "archive"),
        (pin("8.1.0", &sha256.to_string(), CORE_SHA256), "executable"),
    ] {
        let target = root.path().join(label);
        fs::create_dir(&target).unwrap();
        let result = install_archive(&pin, LINUX_X64, &release, sha256, &target, false).await;
        assert_eq!(app_error(result), (MISMATCH.to_owned(), 502), "{label}");
        assert!(!target.join("manifest.json").exists(), "{label}");
    }
    let empty = archive(&root.path().join("empty"), &[("README.md", "x")]);
    let sha256 = digest(&empty);
    let target = root.path().join("no-member");
    fs::create_dir(&target).unwrap();
    let result = install_archive(
        &pin("8.1.0", &sha256.to_string(), CORE_SHA256),
        LINUX_X64,
        &empty,
        sha256,
        &target,
        false,
    )
    .await;
    assert_eq!(
        app_error(result),
        ("The core archive has no cli-proxy-api.".to_owned(), 502)
    );
}

#[test]
fn staging_lives_in_owner_only_cores_and_commits_by_replacing_the_version() {
    let root = TempDir::new("staging");
    let cores = root.path().join("cores");
    let staging = Staging::create(&cores).unwrap();
    assert_eq!(mode(&cores), 0o700);
    assert_eq!(mode(staging.path()), 0o700);
    let name = staging
        .path()
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        name.starts_with(".staging-") && name.len() == ".staging-".len() + 6,
        "{name}"
    );
    fs::write(staging.path().join("cli-proxy-api"), "new").unwrap();
    fs::create_dir(cores.join("9.0.0")).unwrap();
    fs::write(cores.join("9.0.0").join("cli-proxy-api"), "old").unwrap();
    let binary = staging.commit(Version::parse("9.0.0").unwrap()).unwrap();
    assert_eq!(binary, cores.join("9.0.0").join("cli-proxy-api"));
    assert_eq!(fs::read_to_string(&binary).unwrap(), "new");
    // Uncommitted staging is deleted when dropped, as the TypeScript finally did.
    let abandoned = Staging::create(&cores).unwrap();
    let path = abandoned.path().to_owned();
    drop(abandoned);
    assert!(!path.exists());
    let names: Vec<_> = fs::read_dir(&cores)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["9.0.0"]);
    prune(&cores, None).unwrap();
    assert_eq!(fs::read_dir(&cores).unwrap().count(), 0);
}

#[test]
fn startup_selects_the_newest_intact_core_above_the_bundled_one_and_deletes_the_rest() {
    let root = TempDir::new("select");
    let directory = root.path();
    let core = |name: &str, manifest: serde_json::Value, body: &str| {
        fs::create_dir(directory.join(name)).unwrap();
        fs::write(directory.join(name).join("cli-proxy-api"), body).unwrap();
        fs::write(
            directory.join(name).join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();
    };
    let intact = |version: &str| serde_json::json!({ "version": version, "platform": "linux", "arch": "x64", "binarySha256": CORE_SHA256 });
    let with = |version: &str, key: &str, value: &str| {
        let mut manifest = intact(version);
        manifest[key] = value.into();
        manifest
    };
    core("9.0.0", with("9.0.0", "platform", "darwin"), "core");
    core("8.6.0", with("8.6.0", "arch", "arm64"), "core");
    core("8.5.0", intact("8.5.0"), "tampered");
    core("8.4.0", intact("8.3.0"), "core");
    core("8.2.0", intact("8.2.0"), "core");
    core("8.1.0", intact("8.1.0"), "core");
    core("8.0.67", intact("8.0.67"), "core");
    core("8.0.1", intact("8.0.1"), "core");
    fs::create_dir(directory.join(".staging-x1")).unwrap();
    fs::write(directory.join("8.3.0"), "not a directory").unwrap();
    let bundled = Version::parse("8.0.67").unwrap();
    assert_eq!(
        select_installed_core(directory, bundled, LINUX_X64).unwrap(),
        Some(ActiveCore {
            version: Version::parse("8.2.0").unwrap(),
            binary: directory.join("8.2.0").join("cli-proxy-api"),
        })
    );
    let names: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["8.2.0"]);
    assert_eq!(
        select_installed_core(&directory.join("none"), bundled, LINUX_X64).unwrap(),
        None
    );
}

/// Downloads the bundled pin's release for this machine from GitHub: TLS, the redirect to
/// GitHub's asset host, the streamed archive hash and both pin checks.
#[tokio::test]
#[ignore = "downloads about 25 MB from github.com"]
async fn installs_the_bundled_release_from_github() {
    let root = TempDir::new("github");
    let https = nsv_net::Https::new().unwrap();
    let pin = bundled_pin().unwrap();
    let target = CoreTarget::current().unwrap();
    install_core(
        &https,
        nsv_core::install::CORE_ARCHIVES,
        pin,
        target,
        root.path(),
        true,
    )
    .await
    .unwrap();
    nsv_core::verify_core(&root.path().join("cli-proxy-api"), Some(pin)).unwrap();
    nsv_core::check_core_directory(pin, root.path(), target).unwrap();
    assert!(root.path().join("LICENSE").exists());
    assert!(
        !root.path().join(target.archive(&pin.version)).exists(),
        "the archive is deleted"
    );
}
