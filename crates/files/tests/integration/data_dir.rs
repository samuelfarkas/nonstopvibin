use std::ffi::OsStr;
use std::path::Path;

use nsv_domain::id::ProfileId;
use nsv_files::DataDir;

#[test]
fn desktop_and_standalone_default_to_the_home_folder_and_allow_isolated_overrides() {
    let home = std::env::home_dir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert_eq!(DataDir::chosen(OsStr::new("")).unwrap(), None);
    assert_eq!(DataDir::home().unwrap().path(), home.join(".nonstopvibin"));
    assert_eq!(
        DataDir::chosen(OsStr::new(".test-runtime")).unwrap(),
        Some(DataDir::new(cwd.join(".test-runtime")))
    );
    // Lexical, like Node's path.resolve.
    assert_eq!(
        DataDir::chosen(OsStr::new("/data/./a/../b/")).unwrap(),
        Some(DataDir::new("/data/b"))
    );
}

#[test]
fn the_data_directory_layout_keeps_the_names_installed_apps_already_use() {
    let data = DataDir::new("/data");
    let id = ProfileId::parse("00000000-0000-4000-8000-000000000000").unwrap();
    let at = |path: &str| Path::new(path).to_path_buf();
    assert_eq!(data.database(), at("/data/nonstopvibin.sqlite"));
    assert_eq!(data.vault_key(), at("/data/vault.key"));
    assert_eq!(data.profiles(), at("/data/profiles"));
    assert_eq!(data.profile(&id), at(&format!("/data/profiles/{id}")));
    assert_eq!(
        data.credentials(&id),
        at(&format!("/data/profiles/{id}/auth"))
    );
    assert_eq!(
        data.core_config(&id),
        at(&format!("/data/profiles/{id}/config.yaml"))
    );
    assert_eq!(data.oauth_pending(), at("/data/oauth-pending"));
    assert_eq!(data.cores(), at("/data/cores"));
    assert_eq!(data.core_ledger(), at("/data/core-processes"));
    assert_eq!(data.agent_connections(), at("/data/agent-connections"));
    assert_eq!(data.session(), at("/data/server.json"));
    assert_eq!(data.server_lock(), at("/data/server.lock.sqlite"));
    assert_eq!(data.webview(), at("/data/webview"));
}

#[test]
fn the_credential_socket_is_short_and_named_by_the_users_id_and_the_data_directory() {
    let uid = rustix::process::getuid().as_raw();
    // The first 24 hex digits of SHA-256("/data").
    let directory = format!("/tmp/nonstopvibin-{uid}-bd47413b5c03dfc6e4b5b814");
    let data = DataDir::new("/data");
    assert_eq!(data.socket_directory(), Path::new(&directory));
    assert_eq!(
        data.credential_socket(),
        Path::new(&directory).join("agents.sock")
    );
}
