//! Where releases come from: GitHub over HTTPS, following redirects only to its release-asset
//! hosts, or a test feed on loopback HTTP that only a build can name (`NONSTOPVIBIN_UPDATE_FEED`
//! at compile time, read by build.rs). Every read has a size cap.

use std::path::Path;
use std::time::Duration;

use nsv_net::{Download, Get, Https, Loopback, LoopbackRequest, Method, NetError, Redirects};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// `https://github.com/<owner>/<repo>/`, as package.json's publish setting named it.
const REPOSITORY: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/");
/// Release downloads redirect from github.com to these.
const ASSET_HOSTS: &[&str] = &[
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const SMALL: Duration = Duration::from_secs(30);
const PAYLOAD: Duration = Duration::from_mins(30);

pub(crate) enum Source {
    GitHub(Https),
    /// `http://127.0.0.1:<port>/` laid out as GitHub's release paths.
    Test(Loopback),
}

impl Source {
    /// GitHub, unless this build names a test feed. `Err` holds what to log.
    pub(crate) fn new() -> Result<Self, String> {
        match option_env!("NSV_UPDATE_FEED") {
            None => Https::new()
                .map(Self::GitHub)
                .map_err(|error| error.to_string()),
            Some(feed) => feed
                .strip_prefix("http://127.0.0.1:")
                .and_then(|rest| rest.strip_suffix('/'))
                .and_then(|port| port.parse().ok())
                .map(|port| Self::Test(Loopback::v4(port)))
                .ok_or_else(|| {
                    "the build's test update feed is not http://127.0.0.1:<port>/".to_owned()
                }),
        }
    }

    /// A small file at `path` below the repository, read whole.
    pub(crate) async fn get(
        &self,
        path: &str,
        accept: &str,
        limit: u64,
    ) -> Result<Vec<u8>, NetError> {
        let headers = [("Accept", accept)];
        let response = match self {
            Self::GitHub(https) => {
                https
                    .get(&Get {
                        url: &format!("{REPOSITORY}{path}"),
                        headers: &headers,
                        timeout: SMALL,
                        limit,
                        redirects: Redirects::Follow(ASSET_HOSTS),
                    })
                    .await?
            }
            Self::Test(to) => loopback(*to, path, &headers, SMALL, limit).await?,
        };
        if response.ok() {
            Ok(response.body)
        } else {
            Err(NetError::Status(response.status))
        }
    }

    /// Streams the file at `path` into a new owner-only file `to`, with its SHA-256 and size.
    pub(crate) async fn download(
        &self,
        path: &str,
        to: &Path,
        limit: u64,
    ) -> Result<Download, NetError> {
        match self {
            Self::GitHub(https) => {
                let request = Get {
                    url: &format!("{REPOSITORY}{path}"),
                    headers: &[],
                    timeout: PAYLOAD,
                    limit,
                    redirects: Redirects::Follow(ASSET_HOSTS),
                };
                https.download(&request, to, 0o600).await
            }
            Self::Test(from) => {
                let response = loopback(*from, path, &[], PAYLOAD, limit).await?;
                if !response.ok() {
                    return Err(NetError::Status(response.status));
                }
                let mut file = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(to)
                    .await
                    .map_err(NetError::File)?;
                file.write_all(&response.body)
                    .await
                    .map_err(NetError::File)?;
                file.flush().await.map_err(NetError::File)?;
                Ok(Download {
                    sha256: Sha256::digest(&response.body).into(),
                    size: u64::try_from(response.body.len()).unwrap_or(u64::MAX),
                })
            }
        }
    }
}

async fn loopback(
    to: Loopback,
    path: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
    limit: u64,
) -> Result<nsv_net::Response, NetError> {
    nsv_net::loopback(LoopbackRequest {
        to,
        method: &Method::GET,
        target: &format!("/{path}"),
        headers,
        body: None,
        timeout,
        limit,
    })
    .await
}
