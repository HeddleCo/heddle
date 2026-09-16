//! v1 `HostedRoutes` is gone. Callers use `thread_api::rpc::*` through
//! `HostedClient::native()`.

use super::HostedClient;

/// Empty marker so existing `routes()` call sites fail to compile until removed.
pub struct HostedRoutes<'a> {
    _client: &'a HostedClient,
}

impl<'a> HostedRoutes<'a> {
    pub(super) fn new(client: &'a HostedClient) -> Self {
        Self { _client: client }
    }
}
