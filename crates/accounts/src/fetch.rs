//! The outgoing requests of pricing, the model catalog, quota, identity checks, model discovery
//! and core updates, behind one seam so their rules are tested without a network. The daemon passes its
//! [`Https`] client; each caller sets the timeout and body cap `nsv_net` lists. Redirects are
//! refused, as fetch's `redirect: "error"`.

use std::time::Duration;

use nsv_core::install::Releases;
use nsv_core::pool::BoxFuture;
use nsv_net::{Get, Https, Loopback, LoopbackRequest, Method, NetError, Redirects, Response};

/// A GET. Owned, so a request future stays `Send` for spawned background refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub timeout: Duration,
    pub limit: u64,
}

/// What the account features reach outside the daemon: providers over HTTPS or this machine's
/// loopback interface, and the core's pin feed and release archives ([`Releases`]). One
/// object serves every feature.
pub trait Outside: Releases {
    /// To `request.url` over HTTPS, as [`Https::get`].
    fn https(&self, request: Request) -> BoxFuture<'_, Result<Response, NetError>>;

    /// To a provider on this machine's loopback interface, as [`nsv_net::loopback`];
    /// `target` is the path and query.
    fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, NetError>>;
}

impl Outside for Https {
    fn https(&self, request: Request) -> BoxFuture<'_, Result<Response, NetError>> {
        Box::pin(async move {
            let headers: Vec<(&str, &str)> = request
                .headers
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            self.get(&Get {
                url: &request.url,
                headers: &headers,
                timeout: request.timeout,
                limit: request.limit,
                redirects: Redirects::Refuse,
            })
            .await
        })
    }

    fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> BoxFuture<'_, Result<Response, NetError>> {
        Box::pin(async move {
            let headers: Vec<(&str, &str)> = request
                .headers
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            nsv_net::loopback(LoopbackRequest {
                to,
                method: &Method::GET,
                target: &target,
                headers: &headers,
                body: None,
                timeout: request.timeout,
                limit: request.limit,
            })
            .await
        })
    }
}
