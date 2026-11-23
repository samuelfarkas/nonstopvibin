//! The outgoing requests of pricing, the model catalog, `OpenCode` Go quota and model discovery,
//! behind one seam so their rules are tested without a network. The daemon passes its
//! [`Https`] client; each caller sets the timeout and body cap `nsv_net` lists. Redirects are
//! refused, as fetch's `redirect: "error"`.

use std::future::Future;
use std::time::Duration;

use nsv_net::{Get, Https, Loopback, LoopbackRequest, Method, NetError, Redirects, Response};

/// A GET. Owned, so a request future stays `Send` for spawned background refreshes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub timeout: Duration,
    pub limit: u64,
}

pub trait Fetch: Send + Sync {
    /// To `request.url` over HTTPS, as [`Https::get`].
    fn https(&self, request: Request) -> impl Future<Output = Result<Response, NetError>> + Send;

    /// To a provider on this machine's loopback interface, as [`nsv_net::loopback`];
    /// `target` is the path and query.
    fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> impl Future<Output = Result<Response, NetError>> + Send;
}

impl Fetch for Https {
    async fn https(&self, request: Request) -> Result<Response, NetError> {
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
    }

    async fn loopback(
        &self,
        to: Loopback,
        target: String,
        request: Request,
    ) -> Result<Response, NetError> {
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
    }
}
