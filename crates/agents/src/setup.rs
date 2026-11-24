//! `AgentSetup`: connects, inspects, restores and disconnects agents. Each operation's file work
//! runs on a blocking thread; the credential socket runs on the runtime.
//!
//! Layout of the agent connections directory: `<slug>/<agent>.json` manifests (one per project
//! for Claude Code, each with its `.settings.json`), `<slug>/<agent>-key-<port>` helpers and
//! pi's `pi-preferences`. A port change installs a new helper and leaves the old one failing
//! closed for agents still running with it.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool};
use std::{env, io};

use nsv_domain::error::AppError;
use nsv_domain::id::ProfileId;
use nsv_domain::registry::{Agent, registry};
use nsv_domain::secret::ClientKey;
use nsv_domain::sha256_hex;
use nsv_domain::wire::{self, AgentSetupInput, Profile};
use nsv_files::{DataDir, write_atomic};
use tokio::sync::Mutex;

use crate::connection::{Context, XDG_CONFIG_HOME, definition, native_provider, text};
use crate::credential::credential_helper;
use crate::error::{AgentError, conflicting, invalid, refused};
use crate::files::{
    FILE_LIMIT, config_path, json_object, private_directory, read_config, remove_file,
    replace_config,
};
use crate::manifest::{Manifest, validate_models, validate_setup};
use crate::socket::{self, Server};

/// What the credential socket answers and the port agents use now, for a running profile.
pub struct Connection {
    pub key: ClientKey,
    pub port: u16,
}

/// Answers for a profile while it runs and refuses otherwise; the refusal reaches the routes as
/// is. It runs on the socket task and on blocking threads, so it must not block.
pub type ConnectionSource = Arc<dyn Fn(&ProfileId) -> Result<Connection, AppError> + Send + Sync>;

/// The home directory, and the agents' configuration directories that were moved from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDirectories {
    pub home: PathBuf,
    /// Where the environment moved an agent's directory ([`agent_environment`]).
    ///
    /// [`agent_environment`]: crate::agent_environment
    pub moved: BTreeMap<Agent, PathBuf>,
}

impl AgentDirectories {
    /// `home`, with every agent's directory where it is by default.
    #[must_use]
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            moved: BTreeMap::new(),
        }
    }

    /// The user's home, and each agent's directory that its variable or `$XDG_CONFIG_HOME`
    /// moves.
    ///
    /// # Errors
    /// When there is no home directory.
    pub fn from_environment() -> io::Result<Self> {
        let home = env::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory"))?;
        Ok(Self {
            home,
            moved: Agent::ALL
                .iter()
                .filter_map(|&agent| Some((agent, configured(agent)?)))
                .collect(),
        })
    }

    fn custom(&self, agent: Agent) -> Option<&Path> {
        self.moved.get(&agent).map(PathBuf::as_path)
    }
}

/// `$variable`, else `$XDG_CONFIG_HOME/<xdg>` for agents that follow it; empty values are unset.
fn configured(agent: Agent) -> Option<PathBuf> {
    let directory = definition(agent).directory.as_ref()?;
    let set = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
    set(directory.variable)
        .map(PathBuf::from)
        .or_else(|| Some(Path::new(&set(XDG_CONFIG_HOME)?).join(directory.xdg?)))
}

/// The first 24 hexadecimal digits of SHA-256: Claude manifest names.
fn short_hash(bytes: &[u8]) -> String {
    let mut hex = sha256_hex(bytes);
    hex.truncate(24);
    hex
}

pub struct AgentSetup {
    inner: Arc<Inner>,
    server: Mutex<Option<Server>>,
}

struct Inner {
    directory: PathBuf,
    socket_directory: PathBuf,
    socket_path: PathBuf,
    pi_preferences: PathBuf,
    directories: AgentDirectories,
    connection: ConnectionSource,
    /// Serializes changes to shared configuration files.
    busy: AtomicBool,
}

impl AgentSetup {
    /// Agent setup for the data directory `data`. Nothing is created until a connection is made
    /// or restored.
    #[must_use]
    pub fn new(
        data: &DataDir,
        connection: ConnectionSource,
        directories: AgentDirectories,
    ) -> Self {
        let directory = data.agent_connections();
        Self {
            inner: Arc::new(Inner {
                pi_preferences: directory.join("pi-preferences"),
                directory,
                socket_path: data.credential_socket(),
                socket_directory: data.socket_directory(),
                directories,
                connection,
                busy: AtomicBool::new(false),
            }),
            server: Mutex::new(None),
        }
    }

    /// At startup, rewrites connections whose files are still exactly ours but stale (another
    /// port, profile name, template or manifest version), for every profile given, running or
    /// not. Anything removed or edited is left alone and reported for the user to resolve in
    /// Connect agents.
    ///
    /// # Errors
    /// When the credential socket cannot start, for example because another instance serves it.
    pub async fn restore(
        &self,
        profiles: &[Profile],
        port: u16,
    ) -> Result<Vec<String>, AgentError> {
        if blocking(&self.inner, |inner| {
            private_directory(&inner.directory, false)
        })
        .await?
        {
            self.start().await?;
        }
        let profiles = profiles.to_vec();
        blocking(&self.inner, move |inner| Ok(inner.restore(&profiles, port))).await
    }

    /// The connection, with `needsReconnect` when the agent would fail or keep stale settings as
    /// installed: the user must act. `None` when the agent is not connected.
    ///
    /// # Errors
    /// The connection source's refusal for a stopped profile, or an unreadable connection.
    pub async fn status(
        &self,
        profile: &Profile,
        agent: Agent,
        project: Option<&str>,
    ) -> Result<Option<wire::AgentSetup>, AgentError> {
        let (profile, project) = (profile.clone(), project.map(str::to_owned));
        blocking(&self.inner, move |inner| {
            inner.status(&profile, agent, project.as_deref())
        })
        .await
    }

    /// Connects an agent from the app with the models it is offered.
    ///
    /// # Errors
    /// Invalid input (400), a stopped profile, a concurrent change or a conflicting file (409).
    pub async fn install(
        &self,
        profile: &Profile,
        input: AgentSetupInput,
        models: Vec<String>,
    ) -> Result<wire::AgentSetup, AgentError> {
        validate_setup(&input)?;
        validate_models(&models)?;
        let _busy = Busy::acquire(&self.inner.busy)?;
        let profile = profile.clone();
        let plan = blocking(&self.inner, move |inner| {
            let port = || Ok((inner.connection)(&profile.id)?.port);
            inner.prepare(&profile, input, models, &port)
        })
        .await?;
        self.start().await?;
        blocking(&self.inner, move |inner| inner.commit(&plan)).await
    }

    /// Disconnects an agent, removing only what this app installed.
    ///
    /// # Errors
    /// A concurrent change, files edited outside the app (409), or a Claude Code connection in
    /// several projects without a project chosen.
    pub async fn remove(
        &self,
        profile: &Profile,
        agent: Agent,
        project: Option<&str>,
    ) -> Result<(), AgentError> {
        let _busy = Busy::acquire(&self.inner.busy)?;
        let (profile, project) = (profile.clone(), project.map(str::to_owned));
        blocking(&self.inner, move |inner| {
            inner.remove(&profile, agent, project.as_deref())
        })
        .await
    }

    /// Stops the credential socket and removes its file. Helpers fail closed until it restarts.
    ///
    /// # Errors
    /// When the socket file cannot be removed.
    pub async fn close(&self) -> Result<(), AgentError> {
        match self.server.lock().await.take() {
            Some(server) => Ok(server.close().await?),
            None => Ok(()),
        }
    }

    async fn start(&self) -> Result<(), AgentError> {
        let mut server = self.server.lock().await;
        if server.is_none() {
            private_directory(&self.inner.socket_directory, true)?;
            *server = Some(
                socket::serve(&self.inner.socket_path, Arc::clone(&self.inner.connection)).await?,
            );
        }
        Ok(())
    }
}

/// Runs file work on a blocking thread. A panic there continues in the caller's task.
async fn blocking<T, F>(inner: &Arc<Inner>, work: F) -> Result<T, AgentError>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> Result<T, AgentError> + Send + 'static,
{
    let inner = Arc::clone(inner);
    match tokio::task::spawn_blocking(move || work(&inner)).await {
        Ok(result) => result,
        Err(error) => match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(cancelled) => Err(io::Error::other(cancelled).into()),
        },
    }
}

/// Holds the busy flag: one change to shared configuration at a time, as the TypeScript server
/// allowed.
struct Busy<'a>(&'a AtomicBool);

impl<'a> Busy<'a> {
    fn acquire(flag: &'a AtomicBool) -> Result<Self, AgentError> {
        if flag.swap(true, atomic::Ordering::AcqRel) {
            Err(conflicting("Agent setup is already changing. Try again."))
        } else {
            Ok(Self(flag))
        }
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, atomic::Ordering::Release);
    }
}

/// A connection's installed files, read in the order status reports problems in.
struct Installed {
    launch_settings: Option<String>,
    launch_file: Option<String>,
    helper: Option<String>,
    /// The agent's configuration still holds every field the connection wrote.
    config_intact: bool,
}

/// Everything a connection writes, checked against the files before the credential socket
/// starts; the files are compared again as they are replaced.
struct Plan {
    profile: Profile,
    manifest: Manifest,
    manifest_path: PathBuf,
    manifest_text: String,
    helper: PathBuf,
    target: PathBuf,
    before: Option<String>,
    after: Option<String>,
    previous_target: PathBuf,
    previous_content: Option<String>,
    launch_path: PathBuf,
    launch_before: Option<String>,
    launch_after: Option<String>,
}

fn helper_path(folder: &Path, agent: Agent, port: u16) -> PathBuf {
    folder.join(format!("{}-key-{port}", agent.as_str()))
}

fn launch_settings(manifest: Option<&Manifest>) -> Result<Option<String>, AgentError> {
    match manifest
        .and_then(|manifest| Some((manifest, definition(manifest.input.agent).launch_settings?)))
    {
        Some((manifest, settings)) => settings(manifest),
        None => Ok(None),
    }
}

/// An approximation of `localeCompare`, which ordered manifests: case-insensitive first, then
/// lower case before upper case.
fn locale_order(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| right.cmp(left))
}

/// The file a connection's manifest is kept in, in its profile's folder: one per project for an
/// agent connected per project.
#[must_use]
pub fn manifest_name(agent: Agent, project: Option<&str>) -> String {
    match project {
        Some(project) if !project.is_empty() && registry().agents.get(agent).project => {
            format!("{}-{}.json", agent.as_str(), short_hash(project.as_bytes()))
        }
        _ => format!("{}.json", agent.as_str()),
    }
}

fn launch_settings_path(folder: &Path, input: &AgentSetupInput) -> PathBuf {
    let name = manifest_name(input.agent, input.project_directory.as_deref());
    let stem = name.strip_suffix(".json").unwrap_or(&name);
    folder.join(format!("{stem}.settings.json"))
}

impl Inner {
    fn folder(&self, profile: &Profile, create: bool) -> Result<Option<PathBuf>, AgentError> {
        // Older profile names could leave trailing or doubled separators after truncation; a
        // slug still starts with a letter or digit.
        let slug = profile.slug.as_str();
        if !slug.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) {
            return Err(invalid(
                "slug",
                "Invalid string: must match pattern /^[a-z0-9][a-z0-9-]*$/",
            ));
        }
        if !private_directory(&self.directory, create)? {
            return Ok(None);
        }
        let path = self.directory.join(slug);
        Ok(private_directory(&path, create)?.then_some(path))
    }

    fn manifests(&self, profile: &Profile, agent: Agent) -> Result<Vec<Manifest>, AgentError> {
        let Some(folder) = self.folder(profile, false)? else {
            return Ok(Vec::new());
        };
        let names = if registry().agents.get(agent).project {
            let prefix = format!("{}-", agent.as_str());
            let mut names = Vec::new();
            for entry in fs::read_dir(&folder)? {
                let name = entry?.file_name();
                let hashed = name
                    .to_str()
                    .and_then(|name| name.strip_prefix(&prefix)?.strip_suffix(".json"))
                    .is_some_and(|hash| {
                        hash.len() == 24
                            && hash
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    });
                if let Some(name) = name.to_str().filter(|_| hashed) {
                    names.push(name.to_owned());
                }
            }
            names
        } else {
            vec![format!("{}.json", agent.as_str())]
        };
        let mut manifests = Vec::new();
        for name in names {
            let Some(content) = read_config(&folder.join(&name))? else {
                continue;
            };
            let manifest = Manifest::parse(json_object(&content)?)?;
            if manifest.input.agent != agent
                || manifest_name(agent, manifest.input.project_directory.as_deref()) != name
            {
                return Err(refused("Agent setup metadata is invalid."));
            }
            manifests.push(manifest);
        }
        manifests.sort_by(|a, b| {
            locale_order(
                a.input.project_directory.as_deref().unwrap_or_default(),
                b.input.project_directory.as_deref().unwrap_or_default(),
            )
        });
        Ok(manifests)
    }

    fn manifest(
        &self,
        profile: &Profile,
        agent: Agent,
        project: Option<&str>,
    ) -> Result<Option<Manifest>, AgentError> {
        let manifests = self.manifests(profile, agent)?;
        Ok(match project {
            None => manifests.into_iter().next(),
            Some(project) => manifests
                .into_iter()
                .find(|manifest| manifest.input.project_directory.as_deref() == Some(project)),
        })
    }

    fn context<'a>(
        &'a self,
        profile: &'a Profile,
        input: &'a AgentSetupInput,
        helper: &'a Path,
        port: u16,
    ) -> Context<'a> {
        Context {
            profile,
            input,
            port,
            helper,
            socket: &self.socket_path,
            pi_preferences: &self.pi_preferences,
        }
    }

    /// The agent's configuration file for this connection.
    fn target(
        &self,
        profile: &Profile,
        input: &AgentSetupInput,
        create: bool,
        version: u8,
    ) -> Result<PathBuf, AgentError> {
        let definition = definition(input.agent);
        let names = (definition.file)(profile, version);
        if let Some(project_root) = definition.project_root {
            let Some(project) = input
                .project_directory
                .as_deref()
                .filter(|project| !project.is_empty())
            else {
                return Err(refused(format!(
                    "Choose a project for {}.",
                    registry().agents.get(input.agent).title
                )));
            };
            return match project_root(Path::new(project))
                .and_then(|root| config_path(&root, &names, create))
            {
                Err(error) if !create && error.is_missing() => Ok(names
                    .iter()
                    .fold(PathBuf::from(project), |path, name| path.join(name))),
                other => other,
            };
        }
        let custom = self.directories.custom(input.agent);
        let root = custom.unwrap_or(&self.directories.home);
        if create {
            DirBuilder::new().recursive(true).mode(0o700).create(root)?;
        }
        let base = match (custom, &definition.directory) {
            (None, Some(directory)) => directory.home.iter().map(|&name| name.to_owned()).collect(),
            _ => Vec::new(),
        };
        config_path(root, &[base, names].concat(), create)
    }

    fn result(
        &self,
        profile: &Profile,
        manifest: &Manifest,
    ) -> Result<wire::AgentSetup, AgentError> {
        let input = &manifest.input;
        let definition = definition(input.agent);
        let folder = self.directory.join(profile.slug.as_str());
        let launch = launch_settings_path(&folder, input);
        let mut files = vec![
            self.target(profile, input, false, manifest.version)?,
            folder.join(manifest_name(
                input.agent,
                input.project_directory.as_deref(),
            )),
        ];
        if definition.helper {
            files.push(helper_path(&folder, input.agent, manifest.port));
        }
        if launch_settings(Some(manifest))?.is_some_and(|settings| !settings.is_empty()) {
            files.push(launch.clone());
        }
        let projects = if registry().agents.get(input.agent).project {
            let mut projects: Vec<String> = self
                .manifests(profile, input.agent)?
                .into_iter()
                .filter_map(|manifest| manifest.input.project_directory)
                .collect();
            // JavaScript's default sort: UTF-16 code units.
            projects.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Some(projects)
        } else {
            None
        };
        Ok(wire::AgentSetup {
            agent: input.agent,
            project_directory: input.project_directory.clone(),
            other_providers: input.other_providers,
            models: manifest.models.clone(),
            needs_reconnect: None,
            projects,
            command: (definition.command)(profile, &text(&launch)),
            files: files.iter().map(|file| text(file).into_owned()).collect(),
            provider: native_provider(profile),
            instructions: (definition.instructions)(profile),
        })
    }

    fn installed(&self, profile: &Profile, manifest: &Manifest) -> Result<Installed, AgentError> {
        let input = &manifest.input;
        let definition = definition(input.agent);
        let folder = self.directory.join(profile.slug.as_str());
        let launch_settings = launch_settings(Some(manifest))?;
        let launch_file = match launch_settings {
            Some(_) => read_config(&launch_settings_path(&folder, input))?,
            None => None,
        };
        let config = read_config(&self.target(profile, input, false, manifest.version)?)?;
        // Reading fails when the user's settings collide with ours.
        let expected = (definition.merge)(
            config.as_deref(),
            Some(&manifest.content),
            Some(&manifest.content),
        )?;
        let helper = if definition.helper {
            read_config(&helper_path(&folder, input.agent, manifest.port))?
        } else {
            None
        };
        let config_intact = match &config {
            Some(config) => (definition.unchanged)(config, expected.as_deref())?,
            None => false,
        };
        Ok(Installed {
            launch_settings,
            launch_file,
            helper,
            config_intact,
        })
    }

    fn status(
        &self,
        profile: &Profile,
        agent: Agent,
        project: Option<&str>,
    ) -> Result<Option<wire::AgentSetup>, AgentError> {
        let Some(manifest) = self.manifest(profile, agent, project)? else {
            return Ok(None);
        };
        let definition = definition(agent);
        let mut result = self.result(profile, &manifest)?;
        let installed = self.installed(profile, &manifest)?;
        // Like install, status answers only for a running profile: the connection source
        // refuses a stopped one, and the routes answer with its refusal.
        let port = (self.connection)(&profile.id)?.port;
        let helper_current = || {
            installed.helper.as_deref()
                == Some(credential_helper(profile, &self.socket_path, manifest.port).as_str())
        };
        if manifest.version == 1
            || (definition.older_needs_reconnect && manifest.version != definition.version)
            || installed.launch_file != installed.launch_settings
            || !installed.config_intact
            || (definition.helper && !helper_current())
            || port != manifest.port
        {
            result.needs_reconnect = Some(true);
        }
        Ok(Some(result))
    }

    fn restore(&self, profiles: &[Profile], port: u16) -> Vec<String> {
        let mut failures = Vec::new();
        for profile in profiles {
            for &agent in Agent::ALL {
                let Ok(manifests) = self.manifests(profile, agent) else {
                    failures.push(format!(
                        "Could not update {} connection metadata for {}. Open Connect agents to resolve it.",
                        agent.as_str(),
                        profile.slug
                    ));
                    continue;
                };
                for manifest in manifests {
                    if self.restore_connection(profile, &manifest, port).is_err() {
                        failures.push(format!(
                            "Could not automatically update {} for {}. Open Connect agents to resolve edited or missing files.",
                            agent.as_str(),
                            profile.slug
                        ));
                    }
                }
            }
        }
        failures
    }

    fn restore_connection(
        &self,
        profile: &Profile,
        manifest: &Manifest,
        port: u16,
    ) -> Result<(), AgentError> {
        let definition = definition(manifest.input.agent);
        let folder = self.directory.join(profile.slug.as_str());
        let installed = self.installed(profile, manifest)?;
        if !installed.config_intact {
            return Err(refused("Installed connection fields were removed."));
        }
        let helper = helper_path(&folder, manifest.input.agent, port);
        let content =
            (definition.configure)(&self.context(profile, &manifest.input, &helper, port));
        let helper_content = definition
            .helper
            .then(|| credential_helper(profile, &self.socket_path, port));
        if definition.helper && installed.helper.is_none() {
            return Err(refused("The installed credential helper was removed."));
        }
        // A helper from an earlier template or port is still ours to replace.
        if let Some(existing) = &installed.helper {
            let previous = manifest
                .helper_content
                .clone()
                .unwrap_or_else(|| credential_helper(profile, &self.socket_path, manifest.port));
            if Some(existing) != helper_content.as_ref() && *existing != previous {
                return Err(refused("The installed credential helper was edited."));
            }
        }
        if installed.launch_settings.is_some() && installed.launch_file.is_none() {
            return Err(refused("The installed model settings were removed."));
        }
        if manifest.version != definition.version
            || manifest.port != port
            || manifest.content != content
            || manifest.helper_content != helper_content
            || installed.helper != helper_content
            || installed.launch_file != installed.launch_settings
        {
            validate_setup(&manifest.input)?;
            validate_models(&manifest.models)?;
            let _busy = Busy::acquire(&self.busy)?;
            let plan = self.prepare(
                profile,
                manifest.input.clone(),
                manifest.models.clone(),
                &|| Ok(port),
            )?;
            self.commit(&plan)?;
        }
        Ok(())
    }

    /// Checks a connection against the files and builds what it writes. `gateway_port` is asked
    /// once the inputs and the existing manifest are checked.
    fn prepare(
        &self,
        profile: &Profile,
        mut input: AgentSetupInput,
        models: Vec<String>,
        gateway_port: &dyn Fn() -> Result<u16, AgentError>,
    ) -> Result<Plan, AgentError> {
        let definition = definition(input.agent);
        if let (Some(project), Some(project_root)) = (
            input.project_directory.as_deref().filter(|p| !p.is_empty()),
            definition.project_root,
        ) {
            input.project_directory = Some(text(&project_root(Path::new(project))?).into_owned());
        }
        let previous = self.manifest(profile, input.agent, input.project_directory.as_deref())?;
        let port = gateway_port()?;
        if port == 0 {
            return Err(invalid("port", "Too small: expected number to be >=1"));
        }
        let folder = self
            .folder(profile, true)?
            .ok_or_else(|| refused("Could not create the agent connection directory."))?;
        let helper = helper_path(&folder, input.agent, port);
        let helper_content = definition
            .helper
            .then(|| credential_helper(profile, &self.socket_path, port));
        let content = (definition.configure)(&self.context(profile, &input, &helper, port));
        let mut manifest = Manifest {
            version: definition.version,
            input,
            models,
            port,
            content,
            retained: None,
            helper_content,
        };
        let target = self.target(profile, &manifest.input, true, definition.version)?;
        let before = read_config(&target)?;
        let previous_target = match &previous {
            Some(previous) => self.target(profile, &previous.input, false, previous.version)?,
            None => target.clone(),
        };
        let previous_content = if previous_target == target {
            None
        } else {
            read_config(&previous_target)?
        };
        let previous_text = previous.as_ref().map(|previous| previous.content.as_str());
        // Only pi's file moved between manifest versions (see its definition).
        if previous_content.is_some() && previous_content.as_deref() != previous_text {
            return Err(conflicting(
                "The old pi extension was edited outside nonstopvibin. Resolve it before reconnecting.",
            ));
        }
        let after = (definition.merge)(before.as_deref(), previous_text, Some(&manifest.content))?;
        if let Some(retain) = definition.retain {
            manifest.retained = match &previous {
                Some(previous) => previous.retained.clone(),
                None => retain(before.as_deref(), &manifest.content)?,
            };
        }
        let launch_path = launch_settings_path(&folder, &manifest.input);
        let launch_after = launch_settings(Some(&manifest))?;
        let launch_before = match launch_after {
            Some(_) => read_config(&launch_path)?,
            None => None,
        };
        if let Some(existing) = &launch_before
            && Some(existing) != launch_settings(previous.as_ref())?.as_ref()
            && Some(existing) != launch_after.as_ref()
        {
            return Err(conflicting(
                "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before syncing.",
            ));
        }
        let manifest_text = manifest.text();
        if manifest_text.len() > FILE_LIMIT
            || launch_after.as_ref().map_or(0, String::len) > FILE_LIMIT
        {
            return Err(refused(
                "The model catalog is too large for native agent settings.",
            ));
        }
        Ok(Plan {
            manifest_path: folder.join(manifest_name(
                manifest.input.agent,
                manifest.input.project_directory.as_deref(),
            )),
            profile: profile.clone(),
            manifest,
            manifest_text,
            helper,
            target,
            before,
            after,
            previous_target,
            previous_content,
            launch_path,
            launch_before,
            launch_after,
        })
    }

    /// Writes a planned connection; the configuration is put back if a later write fails.
    fn commit(&self, plan: &Plan) -> Result<wire::AgentSetup, AgentError> {
        if let Some(content) = &plan.manifest.helper_content {
            // Refuses symbolic links and files of others before replacing.
            read_config(&plan.helper)?;
            write_atomic(&plan.helper, content.as_bytes(), 0o700)?;
        }
        replace_config(&plan.target, plan.before.as_deref(), plan.after.as_deref())?;
        let mut previous_removed = false;
        if let Err(error) = Self::commit_rest(plan, &mut previous_removed) {
            if previous_removed {
                replace_config(
                    &plan.previous_target,
                    None,
                    plan.previous_content.as_deref(),
                )?;
            }
            replace_config(&plan.target, plan.after.as_deref(), plan.before.as_deref())?;
            return Err(error);
        }
        self.result(&plan.profile, &plan.manifest)
    }

    fn commit_rest(plan: &Plan, previous_removed: &mut bool) -> Result<(), AgentError> {
        if let Some(previous) = &plan.previous_content {
            replace_config(&plan.previous_target, Some(previous), None)?;
            *previous_removed = true;
        }
        if let Some(after) = &plan.launch_after {
            replace_config(
                &plan.launch_path,
                plan.launch_before.as_deref(),
                Some(after),
            )?;
        }
        if let Err(error) = write_atomic(&plan.manifest_path, plan.manifest_text.as_bytes(), 0o600)
        {
            if let Some(after) = &plan.launch_after {
                replace_config(
                    &plan.launch_path,
                    Some(after),
                    plan.launch_before.as_deref(),
                )?;
            }
            return Err(error.into());
        }
        Ok(())
    }

    fn remove(
        &self,
        profile: &Profile,
        agent: Agent,
        project: Option<&str>,
    ) -> Result<(), AgentError> {
        let definition = definition(agent);
        if registry().agents.get(agent).project
            && project.is_none()
            && self.manifests(profile, agent)?.len() > 1
        {
            return Err(refused("Choose which Claude project to disconnect."));
        }
        let Some(manifest) = self.manifest(profile, agent, project)? else {
            return Ok(());
        };
        let target = self.target(profile, &manifest.input, false, manifest.version)?;
        let before = read_config(&target)?;
        let folder = self.directory.join(profile.slug.as_str());
        let launch_path = launch_settings_path(&folder, &manifest.input);
        let launch = launch_settings(Some(&manifest))?;
        let launch_before = match launch {
            Some(_) => read_config(&launch_path)?,
            None => None,
        };
        if launch_before.is_some() && launch_before != launch {
            return Err(conflicting(
                "Claude model settings were edited outside nonstopvibin. Resolve the conflicting file before disconnecting.",
            ));
        }
        let restored = before.as_ref().and(manifest.retained.as_deref());
        let after = (definition.merge)(before.as_deref(), Some(&manifest.content), restored)?;
        replace_config(&target, before.as_deref(), after.as_deref())?;
        if let Some(existing) = &launch_before
            && let Err(error) = replace_config(&launch_path, Some(existing), None)
        {
            replace_config(&target, after.as_deref(), before.as_deref())?;
            return Err(error);
        }
        remove_file(&folder.join(manifest_name(
            agent,
            manifest.input.project_directory.as_deref(),
        )))?;
        if definition.helper && self.manifests(profile, agent)?.is_empty() {
            let prefix = format!("{}-key-", agent.as_str());
            for entry in fs::read_dir(&folder)? {
                let name = entry?.file_name();
                let helper = name
                    .to_str()
                    .and_then(|name| name.strip_prefix(&prefix))
                    .is_some_and(|port| {
                        (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
                    });
                if helper {
                    remove_file(&folder.join(&name))?;
                }
            }
        }
        Ok(())
    }
}
