//! A separate, opt-in testing tool. No production crate depends on this crate.
pub mod config;
pub mod coordinator;
mod journal;
pub mod model;
pub mod network;
mod storage;
pub mod worker;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Notify;

pub const WORKER_PORT: u16 = 6666;
pub const TEST_WARNING: &str = "Deliberately damaging data is for testing purposes only. Use disposable test data. This can cause permanent data loss even when n <= m, because existing corruption or other sources of bitrot outside this process may already have consumed some or all of the erasure tolerance.";
pub const LOSS_WARNING: &str = "Damaging more than m shards in the same stripe will result in certain data loss: normal repair cannot reconstruct the affected stripe from the remaining shards in this k+m set.";

/// Stop scheduling new work on either supported shutdown signal.
pub async fn signals(stop: Stop) {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    stop.stop();
}

#[derive(Clone, Default)]
pub struct Stop {
    stopped: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Stop {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }
    pub async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_stopped() {
                return;
            }
            notified.await;
        }
    }
}
