use objects::object::ChangeId;
use proto::ProtocolError;
use repo::Repository;

use super::HostedGrpcClient;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullMaterialization {
    Full,
    Lazy,
}

impl PullMaterialization {
    pub(crate) fn allows_partial_fetch(self) -> bool {
        matches!(self, Self::Lazy)
    }
}

impl HostedGrpcClient {
    pub async fn hydrate_pulled_state(
        &mut self,
        repo: &Repository,
        repo_path: &str,
        remote_thread: &str,
        target_state: ChangeId,
    ) -> Result<usize, ProtocolError> {
        self.hydrate_missing_blobs_for_state(repo, repo_path, remote_thread, target_state)
            .await
    }
}
