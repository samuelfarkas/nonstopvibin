use std::ffi::{OsStr, OsString};
use std::path::Path;

use nsv_domain::id::ProfileId;
use nsv_store::layout::{
    agent_connections_directory, core_arguments, core_config_path, core_ledger_directory,
    cores_directory, credential_directory, data_directory_from, database_path,
    oauth_pending_directory, profile_directory, server_lock_path, session_path, vault_key_path,
};

#[test]
fn desktop_and_standalone_default_to_the_home_folder_and_allow_isolated_overrides() {
    let home = std::env::home_dir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(
        data_directory_from(Some(OsStr::new(""))).unwrap(),
        home.join(".nonstopvibin")
    );
    assert_eq!(
        data_directory_from(None).unwrap(),
        home.join(".nonstopvibin")
    );
    assert_eq!(
        data_directory_from(Some(OsStr::new(".test-runtime"))).unwrap(),
        cwd.join(".test-runtime")
    );
    // Lexical, like Node's path.resolve.
    assert_eq!(
        data_directory_from(Some(OsStr::new("/data/./a/../b/"))).unwrap(),
        Path::new("/data/b")
    );
}

#[test]
fn the_data_directory_layout_keeps_the_names_installed_apps_already_use() {
    let data = Path::new("/data");
    let id = ProfileId::parse("00000000-0000-4000-8000-000000000000").unwrap();
    let at = |path: &str| Path::new(path).to_path_buf();
    assert_eq!(database_path(data), at("/data/nonstopvibin.sqlite"));
    assert_eq!(vault_key_path(data), at("/data/vault.key"));
    assert_eq!(
        profile_directory(data, &id),
        at(&format!("/data/profiles/{id}"))
    );
    assert_eq!(
        credential_directory(data, &id),
        at(&format!("/data/profiles/{id}/auth"))
    );
    assert_eq!(
        core_config_path(data, &id),
        at(&format!("/data/profiles/{id}/config.yaml"))
    );
    assert_eq!(
        core_arguments(data, &id),
        [
            OsString::from("--config"),
            OsString::from(format!("/data/profiles/{id}/config.yaml"))
        ]
    );
    assert_eq!(oauth_pending_directory(data), at("/data/oauth-pending"));
    assert_eq!(cores_directory(data), at("/data/cores"));
    assert_eq!(core_ledger_directory(data), at("/data/core-processes"));
    assert_eq!(
        agent_connections_directory(data),
        at("/data/agent-connections")
    );
    assert_eq!(session_path(data), at("/data/server.json"));
    assert_eq!(server_lock_path(data), at("/data/server.lock.sqlite"));
}
