//! The daemon's side of Claude Agent: the engine reads a profile's Claude Agent account from
//! the store, and the management API reports and connects it.

use std::path::PathBuf;

use nsv_claude_agent::{Account, Engine, Host, Options};
use nsv_domain::error::AppError;
use nsv_domain::id::{AccountId, ProfileId};
use nsv_domain::registry::Provider;
use nsv_files::DataDir;
use nsv_store::Store;

/// The engine's view of the store: the profile's enabled Claude Agent account and its key
/// from the vault.
pub(crate) struct Accounts(pub(crate) Store);

impl Host for Accounts {
    async fn account(&self, profile: &ProfileId) -> Option<Account> {
        let profile = profile.clone();
        self.0
            .call(move |db| {
                let Some(account) = db
                    .api_accounts(&profile)?
                    .into_iter()
                    .find(|account| account.provider == Provider::ClaudeAgent && !account.disabled)
                else {
                    return Ok(None);
                };
                Ok(Some(Account {
                    key: db.api_key(&account.id)?,
                }))
            })
            .await
            .ok()
            .flatten()
    }
}

pub(crate) type ClaudeAgent = Engine<Accounts>;

/// The engine for the daemon on `port`, running `program`, or the `claude` it finds.
pub(crate) fn engine(
    store: &Store,
    data: &DataDir,
    port: u16,
    program: Option<PathBuf>,
) -> ClaudeAgent {
    Engine::new(
        Accounts(store.clone()),
        Options {
            data: data.clone(),
            port,
            program,
        },
    )
}

/// Whether `account` is `profile`'s Claude Agent account.
pub(crate) async fn is_claude_agent(
    store: &Store,
    profile: &ProfileId,
    account: &AccountId,
) -> Result<bool, AppError> {
    let (profile, account) = (profile.clone(), account.clone());
    Ok(store
        .call(move |db| {
            Ok(db
                .api_accounts(&profile)?
                .iter()
                .any(|known| known.id == account && known.provider == Provider::ClaudeAgent))
        })
        .await?)
}
