use rmcp::{
    model::{ProgressNotificationParam, ProgressToken},
    service::{Peer, RequestContext, RoleServer},
};
use tokio_util::sync::CancellationToken;

/// Best-effort progress reporting for synchronous tool calls.
#[derive(Clone, Debug)]
pub struct ProgressReporter {
    peer: Peer<RoleServer>,
    token: Option<ProgressToken>,
    cancellation: CancellationToken,
}

impl ProgressReporter {
    pub fn from_context(context: &RequestContext<RoleServer>) -> Self {
        Self {
            peer: context.peer.clone(),
            token: context.meta.get_progress_token(),
            cancellation: context.ct.clone(),
        }
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    pub async fn report(&self, progress: f64, total: Option<f64>, message: impl Into<String>) {
        let Some(token) = self.token.clone() else {
            return;
        };
        if self.cancellation.is_cancelled() {
            return;
        }
        let mut notification = ProgressNotificationParam::new(token, progress);
        if let Some(total) = total {
            notification = notification.with_total(total);
        }
        let notification = notification.with_message(message.into());
        let _ = self.peer.notify_progress(notification).await;
    }
}
