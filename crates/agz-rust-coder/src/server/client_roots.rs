use std::{fmt, path::Path, sync::Arc, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use rmcp::{
    model::{ClientCapabilities, ClientResult, ServerRequest},
    service::{Peer, PeerRequestOptions, RoleServer},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::workspace::{ClientRoots, RootError, RootGuard, WorkspaceRoot};

const ROOTS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

type RootFetchResult = Result<ClientRoots, RootError>;
type RootFetch = futures::future::Shared<BoxFuture<'static, RootFetchResult>>;

struct RootEpochState {
    epoch: u64,
    cached: Option<RootFetchResult>,
    in_flight: Option<RootFetch>,
    cancellation: CancellationToken,
}

impl RootEpochState {
    fn new() -> Self {
        Self {
            epoch: 0,
            cached: None,
            in_flight: None,
            cancellation: CancellationToken::new(),
        }
    }
}

impl fmt::Debug for RootEpochState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootEpochState")
            .field("epoch", &self.epoch)
            .field("cached", &self.cached.is_some())
            .field("in_flight", &self.in_flight.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) struct ClientRootsCoordinator {
    guard: Arc<RootGuard>,
    state: Mutex<RootEpochState>,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkspaceRequest {
    pub(crate) root: WorkspaceRoot,
    pub(crate) client_roots: ClientRoots,
    epoch_cancellation: CancellationToken,
}

pub(crate) struct CancellationBridge {
    token: CancellationToken,
    waiter: tokio::task::JoinHandle<()>,
}

impl CancellationBridge {
    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for CancellationBridge {
    fn drop(&mut self) {
        self.waiter.abort();
    }
}

impl ClientRootsCoordinator {
    pub(crate) fn new(guard: Arc<RootGuard>) -> Self {
        Self {
            guard,
            state: Mutex::new(RootEpochState::new()),
        }
    }

    pub(crate) async fn resolve(
        &self,
        peer: &Peer<RoleServer>,
        capabilities: Option<&ClientCapabilities>,
        directory: Option<&Path>,
        request_cancellation: &CancellationToken,
    ) -> Result<WorkspaceRequest, RootError> {
        let (client_roots, epoch_cancellation) = if client_supports_roots(capabilities) {
            self.client_roots(peer, request_cancellation).await?
        } else {
            (ClientRoots::unsupported(), self.epoch_cancellation().await)
        };
        let snapshot = self.guard.snapshot(client_roots.clone())?;
        let root = snapshot.select(directory)?;
        Ok(WorkspaceRequest {
            root,
            client_roots,
            epoch_cancellation,
        })
    }

    pub(crate) async fn invalidate(&self) {
        let old_cancellation = {
            let mut state = self.state.lock().await;
            state.epoch = state.epoch.saturating_add(1);
            state.cached = None;
            state.in_flight = None;
            std::mem::replace(&mut state.cancellation, CancellationToken::new())
        };
        old_cancellation.cancel();
        if let Err(error) = self.guard.invalidate_client_roots() {
            tracing::warn!(%error, "could not invalidate client roots snapshot");
        }
    }

    async fn epoch_cancellation(&self) -> CancellationToken {
        self.state.lock().await.cancellation.clone()
    }

    async fn client_roots(
        &self,
        peer: &Peer<RoleServer>,
        request_cancellation: &CancellationToken,
    ) -> Result<(ClientRoots, CancellationToken), RootError> {
        let (epoch, cancellation, fetch) = {
            let mut state = self.state.lock().await;
            if let Some(result) = state.cached.clone() {
                return result.map(|roots| (roots, state.cancellation.clone()));
            }
            if let Some(fetch) = state.in_flight.clone() {
                (state.epoch, state.cancellation.clone(), fetch)
            } else {
                let cancellation = state.cancellation.clone();
                let fetch = fetch_client_roots(peer.clone(), cancellation.clone())
                    .boxed()
                    .shared();
                let background = fetch.clone();
                tokio::spawn(async move {
                    let _ = background.await;
                });
                state.in_flight = Some(fetch.clone());
                (state.epoch, cancellation, fetch)
            }
        };

        let result = tokio::select! {
            result = fetch => result,
            () = request_cancellation.cancelled() => {
                return Err(RootError::ClientRootsUnavailable);
            }
        };
        let mut state = self.state.lock().await;
        if state.epoch != epoch {
            return Err(RootError::ClientRootsUnavailable);
        }
        state.in_flight = None;
        state.cached = Some(result.clone());
        result.map(|roots| (roots, cancellation))
    }
}

impl WorkspaceRequest {
    pub(crate) fn cancellation(
        &self,
        request_cancellation: CancellationToken,
        shutdown_cancellation: CancellationToken,
    ) -> CancellationBridge {
        let token = CancellationToken::new();
        let token_for_waiter = token.clone();
        let epoch_cancellation = self.epoch_cancellation.clone();
        let waiter = tokio::spawn(async move {
            tokio::select! {
                () = request_cancellation.cancelled() => {}
                () = epoch_cancellation.cancelled() => {}
                () = shutdown_cancellation.cancelled() => {}
            }
            token_for_waiter.cancel();
        });
        CancellationBridge { token, waiter }
    }
}

#[allow(deprecated)]
fn client_supports_roots(capabilities: Option<&ClientCapabilities>) -> bool {
    capabilities.is_some_and(|capabilities| capabilities.roots.is_some())
}

#[allow(deprecated)]
async fn fetch_client_roots(
    peer: Peer<RoleServer>,
    epoch_cancellation: CancellationToken,
) -> RootFetchResult {
    let request = ServerRequest::ListRootsRequest(rmcp::model::ListRootsRequest {
        method: Default::default(),
        extensions: Default::default(),
    });
    let handle = peer
        .send_cancellable_request(
            request,
            PeerRequestOptions::with_timeout(ROOTS_REQUEST_TIMEOUT),
        )
        .await
        .map_err(|_| RootError::ClientRootsUnavailable)?;
    let request_id = handle.id.clone();
    let response = tokio::select! {
        response = handle.await_response() => response
            .map_err(|_| RootError::ClientRootsUnavailable)?,
        () = epoch_cancellation.cancelled() => {
            let _ = peer
                .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                    Some(request_id),
                    Some("client roots changed".to_owned()),
                ))
                .await;
            return Err(RootError::ClientRootsUnavailable);
        }
    };
    let ClientResult::ListRootsResult(result) = response else {
        return Err(RootError::ClientRootsUnavailable);
    };
    ClientRoots::from_file_uris(result.roots.into_iter().map(|root| root.uri))
        .map_err(|_| RootError::ClientRootsUnavailable)
}
