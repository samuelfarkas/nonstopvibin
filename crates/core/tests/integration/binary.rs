use std::fs;

use nsv_core::binary::{Binary, CoreManifest, CorePin, CoreTarget, Sha256Digest, bundled_pin};
use nsv_core::{CoreError, check_core_directory, verify_core};
use nsv_domain::version::{AppVersion, Version};
use nsv_testkit::TempDir;

use crate::common::{CORE_SHA256, LINUX_X64, pin};

fn message(result: Result<(), CoreError>) -> (String, u16) {
    match result {
        Err(CoreError::App(error)) => (error.message, error.status),
        other => panic!("expected an AppError, got {other:?}"),
    }
}

#[test]
fn the_bundled_pin_is_the_reviewed_release() {
    let pin = bundled_pin().unwrap();
    assert_eq!(pin.version.to_string(), "8.1.0");
    assert_eq!(pin.min_app, None);
    assert_eq!(
        pin.checksums.get(CoreTarget::LinuxX64).to_string(),
        "09492d9a338eb2d84b5e86f5456af0ff1fa951ad266c3df78e7db571e8ec41b1"
    );
    assert_eq!(
        pin.binaries.get(CoreTarget::DarwinArm64).to_string(),
        "a18ee4edf52ce85c4363cd2e69ea22812b3bb1c64e29b68e9522151e2b1d24ae"
    );
}

#[test]
fn a_pin_ignores_unknown_keys_and_refuses_malformed_values() {
    let base = serde_json::json!({
        "version": "9.0.0",
        "minApp": "0.3.0",
        "later": true,
        "checksums": { "darwin_arm64": CORE_SHA256, "darwin_x64": CORE_SHA256, "linux_arm64": CORE_SHA256, "linux_x64": CORE_SHA256 },
        "binaries": { "darwin_arm64": CORE_SHA256, "darwin_x64": CORE_SHA256, "linux_arm64": CORE_SHA256, "linux_x64": CORE_SHA256 },
    });
    let pin: CorePin = serde_json::from_value(base.clone()).unwrap();
    assert_eq!(pin.min_app, AppVersion::parse("0.3.0"));
    for (pointer, value) in [
        ("/version", serde_json::json!("9.0")),
        ("/minApp", serde_json::json!("v1.0.0")),
        (
            "/checksums/linux_x64",
            serde_json::json!(CORE_SHA256.to_uppercase()),
        ),
        ("/binaries/darwin_x64", serde_json::json!("abc")),
    ] {
        let mut bad = base.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        assert!(serde_json::from_value::<CorePin>(bad).is_err(), "{pointer}");
    }
    let mut missing = base;
    missing["binaries"]
        .as_object_mut()
        .unwrap()
        .remove("linux_arm64");
    assert!(serde_json::from_value::<CorePin>(missing).is_err());
}

#[test]
fn a_pin_is_written_in_the_files_key_order() {
    let file = include_str!("../../core-release.json");
    let pin = bundled_pin().unwrap();
    assert_eq!(serde_json::to_string_pretty(pin).unwrap() + "\n", file);
    let later = CorePin {
        min_app: AppVersion::parse("0.3.0"),
        ..pin.clone()
    };
    let text = serde_json::to_string_pretty(&later).unwrap();
    assert!(
        text.starts_with("{\n  \"version\": \"8.1.0\",\n  \"minApp\": \"0.3.0\",\n  \"checksums\": {\n    \"darwin_arm64\": "),
        "{text}"
    );
}

#[test]
fn targets_use_node_names_and_upstream_archive_names() {
    let version = Version::parse("8.1.0").unwrap();
    let expected = [
        ("darwin", "arm64", "CLIProxyAPI_8.1.0_darwin_aarch64.tar.gz"),
        ("darwin", "x64", "CLIProxyAPI_8.1.0_darwin_amd64.tar.gz"),
        ("linux", "arm64", "CLIProxyAPI_8.1.0_linux_aarch64.tar.gz"),
        ("linux", "x64", "CLIProxyAPI_8.1.0_linux_amd64.tar.gz"),
    ];
    for (platform, arch, archive) in expected {
        let target = CoreTarget::new(platform, arch).unwrap();
        assert_eq!((target.platform(), target.arch()), (platform, arch));
        assert_eq!(target.archive(&version), archive);
    }
    let refused = CoreTarget::parse("win32", "x64").unwrap_err();
    assert_eq!(
        refused.message,
        "Supported core platforms: macOS/Linux arm64 and x64."
    );
    assert_eq!(refused.status, 400);
    assert!(CoreTarget::new("linux", "ia32").is_none());
    assert_eq!(
        CoreTarget::from_key("darwin_x64"),
        Some(CoreTarget::DarwinX64)
    );
    assert_eq!(CoreTarget::from_key("linux-x64"), None);
}

#[test]
fn a_file_digest_is_its_sha256() {
    let directory = TempDir::new("digest");
    let path = directory.path().join("core");
    fs::write(&path, "core").unwrap();
    assert_eq!(
        Sha256Digest::of_file(&path).unwrap().to_string(),
        CORE_SHA256
    );
    assert_eq!(
        Sha256Digest::parse(CORE_SHA256).unwrap().to_string(),
        CORE_SHA256
    );
}

#[test]
fn a_manifest_is_written_as_json_stringify_with_two_spaces_and_read_leniently() {
    let directory = TempDir::new("manifest");
    let pin = pin("8.1.0", CORE_SHA256, CORE_SHA256);
    let digest = Sha256Digest::parse(CORE_SHA256).unwrap();
    CoreManifest::installed(&pin, LINUX_X64, digest, digest)
        .write(directory.path())
        .unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("manifest.json")).unwrap(),
        format!(
            "{{\n  \"version\": \"8.1.0\",\n  \"platform\": \"linux\",\n  \"arch\": \"x64\",\n  \"archive\": \"CLIProxyAPI_8.1.0_linux_amd64.tar.gz\",\n  \"sha256\": \"{CORE_SHA256}\",\n  \"binarySha256\": \"{CORE_SHA256}\"\n}}"
        )
    );
    fs::write(
        directory.path().join("manifest.json"),
        r#"{"version": 8, "binarySha256": "x"}"#,
    )
    .unwrap();
    let read = CoreManifest::read(directory.path()).unwrap();
    assert_eq!(read.version, None);
    assert_eq!(read.binary_sha256.as_deref(), Some("x"));
    fs::write(directory.path().join("manifest.json"), "[1]").unwrap();
    assert_eq!(
        CoreManifest::read(directory.path()).unwrap(),
        CoreManifest::default()
    );
    fs::write(directory.path().join("manifest.json"), "{").unwrap();
    assert!(matches!(
        CoreManifest::read(directory.path()),
        Err(CoreError::Json(_))
    ));
}

/// A core directory with the binary `body` and a manifest of `fields`.
fn core_directory(body: &str, fields: &serde_json::Value) -> TempDir {
    let directory = TempDir::new("check");
    fs::write(directory.path().join("cli-proxy-api"), body).unwrap();
    fs::write(directory.path().join("manifest.json"), fields.to_string()).unwrap();
    directory
}

#[test]
fn check_core_directory_refuses_in_order() {
    let pin = pin("8.1.0", &"a".repeat(64), CORE_SHA256);
    let good = serde_json::json!({
        "version": "8.1.0", "platform": "linux", "arch": "x64",
        "sha256": "a".repeat(64), "binarySha256": CORE_SHA256,
    });
    let check = |body: &str, fields: serde_json::Value, target| {
        let directory = core_directory(body, &fields);
        check_core_directory(&pin, directory.path(), target, Binary::Pinned)
    };
    check("core", good.clone(), LINUX_X64).unwrap();
    let with = |key: &str, value: &str| {
        let mut fields = good.clone();
        fields[key] = value.into();
        fields
    };
    let pin_message = "The proxy core does not match the reviewed release pin.".to_owned();
    assert_eq!(
        message(check("core", with("version", "8.0.0"), LINUX_X64)),
        (pin_message.clone(), 500)
    );
    assert_eq!(
        message(check("core", with("sha256", &"b".repeat(64)), LINUX_X64)),
        (pin_message.clone(), 500)
    );
    assert_eq!(
        message(check("core", with("platform", "win32"), LINUX_X64)),
        (pin_message, 500)
    );
    assert_eq!(
        message(check("core", good.clone(), CoreTarget::DarwinArm64)),
        (
            "The proxy core is linux/x64, but the target is darwin/arm64. Run core:install for the target first."
                .to_owned(),
            500
        )
    );
    assert_eq!(
        message(check("core", with("binarySha256", &"c".repeat(64)), LINUX_X64)),
        (
            "The proxy core manifest does not match the reviewed binary pin. Run bun run core:install again."
                .to_owned(),
            500
        )
    );
    assert_eq!(
        message(check("tampered", good.clone(), LINUX_X64)),
        (
            "The proxy core binary does not match the reviewed release pin. Run bun run core:install again."
                .to_owned(),
            500
        )
    );

    // Signing rewrote the binary and recorded its hash in the manifest.
    let signed = |body: &str, fields: serde_json::Value| {
        let directory = core_directory(body, &fields);
        check_core_directory(&pin, directory.path(), LINUX_X64, Binary::Signed)
    };
    // The SHA-256 of "signed".
    let resigned = with(
        "binarySha256",
        "4a3cdfae6f291c8f544daea5b72905cf9e74c1ed427d831ad0d7ca00c73c794d",
    );
    signed("signed", resigned.clone()).unwrap();
    assert_eq!(
        message(signed("altered", resigned)),
        ("The signed core differs from its manifest.".to_owned(), 500)
    );
    assert_eq!(
        message(signed("signed", with("version", "8.0.0"))),
        (
            "The proxy core does not match the reviewed release pin.".to_owned(),
            500
        )
    );
}

#[test]
fn verify_core_checks_the_manifest_and_optionally_the_pin() {
    let integrity = (
        "The proxy core failed its integrity check. Reinstall NonstopVibin.".to_owned(),
        503,
    );
    // The fake core's manifest carries only the binary hash, as the TypeScript fake core wrote.
    let directory = core_directory("core", &serde_json::json!({ "binarySha256": CORE_SHA256 }));
    let binary = directory.path().join("cli-proxy-api");
    verify_core(&binary, None).unwrap();
    let pinned = pin("8.1.0", CORE_SHA256, CORE_SHA256);
    verify_core(&binary, Some(&pinned)).unwrap();
    let other = pin("8.1.0", CORE_SHA256, &"d".repeat(64));
    assert_eq!(message(verify_core(&binary, Some(&other))), integrity);
    fs::write(&binary, "damaged").unwrap();
    assert_eq!(message(verify_core(&binary, None)), integrity);
    let missing = directory.path().join("missing");
    assert_eq!(
        message(verify_core(&missing, None)),
        (
            format!(
                "The proxy core is missing at {}. Reinstall NonstopVibin.",
                missing.display()
            ),
            503
        )
    );
}
