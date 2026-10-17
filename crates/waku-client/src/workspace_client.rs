use uuid::Uuid;
use waku_protocol::provider_session::{ProviderSessionFork, ProviderSessionForkRequest};
use waku_protocol::{Command, ResponsePayload, WorkspaceOperation, WorkspaceResult};

use crate::{DaemonClient, DaemonSupervisor};

/// Typed convenience wrapper around daemon-owned filesystem and Git RPCs.
#[derive(Clone)]
pub struct WorkspaceClient {
    target: WorkspaceTarget,
}

#[derive(Clone)]
enum WorkspaceTarget {
    Client(DaemonClient),
    Supervisor(DaemonSupervisor),
}

impl WorkspaceClient {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            target: WorkspaceTarget::Client(client),
        }
    }

    /// Capturing a supervisor is lock-free. Resolve its current transport only
    /// when the operation runs on the background executor, after a reconnect
    /// if necessary; rendering must never acquire the supervisor's target lock.
    pub fn for_daemon(daemon: DaemonSupervisor) -> Self {
        Self {
            target: WorkspaceTarget::Supervisor(daemon),
        }
    }

    fn client(&self) -> DaemonClient {
        match &self.target {
            WorkspaceTarget::Client(client) => client.clone(),
            WorkspaceTarget::Supervisor(daemon) => daemon.client(),
        }
    }

    pub fn request(&self, operation: WorkspaceOperation) -> anyhow::Result<WorkspaceResult> {
        match self
            .client()
            .request(Uuid::nil(), Uuid::nil(), Command::Workspace { operation })?
        {
            ResponsePayload::Workspace { result } => Ok(result),
            _ => anyhow::bail!("Waku daemon returned an invalid workspace response"),
        }
    }

    pub fn fork_provider_session(
        &self,
        request: ProviderSessionForkRequest,
    ) -> anyhow::Result<ProviderSessionFork> {
        match self.client().request(
            Uuid::nil(),
            Uuid::nil(),
            Command::ForkProviderSession { request },
        )? {
            ResponsePayload::ProviderSessionForked { result } => Ok(result),
            _ => anyhow::bail!("Waku daemon returned an invalid provider-fork response"),
        }
    }
}
