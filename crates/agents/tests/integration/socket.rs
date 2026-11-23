//! The credential socket's ownership rules: owner-only modes, refusal of a live socket another
//! instance serves, takeover of a stale one, refusal of anything else at its path, and the
//! bounded request head. Its replies are in the goldens.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;

use nsv_agents::AgentSetup;
use nsv_domain::error::AppError;
use nsv_domain::registry::Agent;
use nsv_domain::wire::{AgentSetupInput, Profile};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::common::{Broker, Scratch, directory, home_only, key, profile, run, setup};

struct Instance {
    root: Scratch,
    profile: Profile,
    broker: Broker,
    setup: AgentSetup,
}

impl Instance {
    fn new() -> Self {
        let root = Scratch::new();
        directory(&root.root.join("home"), 0o755);
        let profile = profile("6f1f3c7e-3c1b-4d55-9a2e-0d6a5f6b2c11", "work", "Work");
        let broker = Broker::new(
            4318,
            AppError::new("Start this profile before connecting an agent.", 409),
        );
        broker.start(&profile, &key('A'));
        let setup = setup(&root.root, &broker, home_only(&root.root.join("home")));
        Self {
            root,
            profile,
            broker,
            setup,
        }
    }

    /// A second instance on the same data directory.
    fn second(&self) -> AgentSetup {
        setup(
            &self.root.root,
            &self.broker,
            home_only(&self.root.root.join("home")),
        )
    }

    async fn install(&self, setup: &AgentSetup) -> Result<String, nsv_agents::AgentError> {
        let input = AgentSetupInput {
            agent: Agent::Codex,
            project_directory: None,
            other_providers: None,
        };
        let installed = setup
            .install(&self.profile, input, vec!["m".to_owned()])
            .await?;
        Ok(installed
            .files
            .into_iter()
            .find(|file| file.contains("codex-key-"))
            .unwrap())
    }

    async fn key_of(&self, helper: &str) -> String {
        String::from_utf8(run(helper, &[], &self.root.root).await.stdout).unwrap()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Best effort: the socket directory lives outside the scratch directory.
        drop(fs::remove_dir_all(self.setup.socket_directory()));
    }
}

fn mode(path: &std::path::Path) -> u32 {
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn the_socket_is_owner_only_and_a_live_one_is_not_taken_over() {
    let instance = Instance::new();
    let helper = instance.install(&instance.setup).await.unwrap();
    assert_eq!(mode(instance.setup.socket_directory()), 0o700);
    assert_eq!(mode(instance.setup.socket_path()), 0o600);
    let second = instance.second();
    let refused = second
        .restore(std::slice::from_ref(&instance.profile), 4318)
        .await
        .map_err(AppError::from)
        .unwrap_err();
    assert_eq!(refused.status, 409);
    assert_eq!(
        refused.to_string(),
        "Another nonstopvibin instance is serving these agent connections."
    );
    assert_eq!(
        instance.key_of(&helper).await,
        format!("{}\n", key('A')),
        "the first instance still answers"
    );
    instance.setup.close().await.unwrap();
    assert!(
        !instance.setup.socket_path().exists(),
        "closing removes the socket"
    );
}

#[tokio::test]
async fn a_stale_socket_left_by_a_crash_is_replaced() {
    let instance = Instance::new();
    directory(instance.setup.socket_directory(), 0o700);
    // A socket file nothing listens on any more, as a crashed instance leaves it.
    drop(UnixListener::bind(instance.setup.socket_path()).unwrap());
    assert!(instance.setup.socket_path().exists());
    let helper = instance.install(&instance.setup).await.unwrap();
    assert_eq!(instance.key_of(&helper).await, format!("{}\n", key('A')));
    instance.setup.close().await.unwrap();
}

#[tokio::test]
async fn any_other_file_at_the_socket_path_is_refused() {
    let instance = Instance::new();
    directory(instance.setup.socket_directory(), 0o700);
    fs::write(instance.setup.socket_path(), "not a socket").unwrap();
    let refused = instance.install(&instance.setup).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "The agent connection path is occupied by an unexpected file."
    );
    assert_eq!(
        fs::read_to_string(instance.setup.socket_path()).unwrap(),
        "not a socket"
    );
    fs::remove_file(instance.setup.socket_path()).unwrap();
    directory(instance.setup.socket_directory(), 0o755);
    let refused = instance.install(&instance.setup).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        "Agent setup needs an owner-only directory without symbolic links."
    );
}

#[tokio::test]
async fn an_oversized_request_head_is_refused_without_a_key() {
    let instance = Instance::new();
    instance.install(&instance.setup).await.unwrap();
    let mut stream = UnixStream::connect(instance.setup.socket_path())
        .await
        .unwrap();
    let request = format!(
        "GET /profiles/{} HTTP/1.1\r\nHost: localhost\r\nX-Padding: {}\r\n\r\n",
        instance.profile.id,
        "a".repeat(5000)
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert_eq!(
        reply,
        "HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n"
    );
    instance.setup.close().await.unwrap();
}
