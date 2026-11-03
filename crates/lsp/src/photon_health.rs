//! photon: notices a language server that has stopped answering, so the project can restart it.
//! A busy server keeps sending something (progress, diagnostics, logs), so a request only counts
//! against it when it timed out while the server sent nothing at all.

use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use futures::channel::oneshot;
use parking_lot::Mutex;

/// Requests in a row that time out with the server silent before it counts as unresponsive.
const SILENT_TIMEOUTS: usize = 2;

pub(crate) struct Health {
    /// Messages received from the server so far.
    incoming: AtomicUsize,
    silent_timeouts: AtomicUsize,
    unresponsive_tx: Mutex<Option<oneshot::Sender<()>>>,
    unresponsive_rx: Mutex<Option<oneshot::Receiver<()>>>,
}

impl Default for Health {
    fn default() -> Self {
        let (tx, rx) = oneshot::channel();
        Self {
            incoming: AtomicUsize::new(0),
            silent_timeouts: AtomicUsize::new(0),
            unresponsive_tx: Mutex::new(Some(tx)),
            unresponsive_rx: Mutex::new(Some(rx)),
        }
    }
}

impl Health {
    pub(crate) fn received(&self) {
        self.incoming.fetch_add(1, SeqCst);
    }

    /// A mark to pass to [`Self::timed_out`] for a request sent now.
    pub(crate) fn mark(&self) -> usize {
        self.incoming.load(SeqCst)
    }

    pub(crate) fn timed_out(&self, mark: usize) {
        if self.incoming.load(SeqCst) != mark {
            self.silent_timeouts.store(0, SeqCst);
            return;
        }
        if self.silent_timeouts.fetch_add(1, SeqCst) + 1 >= SILENT_TIMEOUTS {
            self.signal();
        }
    }

    pub(crate) fn signal(&self) {
        if let Some(tx) = self.unresponsive_tx.lock().take() {
            tx.send(()).ok();
        }
    }

    pub(crate) fn answered(&self) {
        self.silent_timeouts.store(0, SeqCst);
    }

    /// Resolves once the server has stopped answering; taken once per server.
    pub(crate) fn take_unresponsive_signal(&self) -> Option<oneshot::Receiver<()>> {
        self.unresponsive_rx.lock().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(health: &Health) -> bool {
        health
            .take_unresponsive_signal()
            .is_some_and(|mut rx| matches!(rx.try_recv(), Ok(Some(()))))
    }

    #[test]
    fn two_silent_timeouts_in_a_row_mean_the_server_stopped_answering() {
        let health = Health::default();
        health.timed_out(health.mark());
        health.timed_out(health.mark());
        assert!(fired(&health));
    }

    #[test]
    fn a_busy_server_that_keeps_talking_is_left_alone() {
        let health = Health::default();
        let mark = health.mark();
        health.received();
        health.timed_out(mark);
        let mark = health.mark();
        health.received();
        health.timed_out(mark);
        assert!(!fired(&health));
    }

    #[test]
    fn an_answer_between_timeouts_starts_the_count_again() {
        let health = Health::default();
        health.timed_out(health.mark());
        health.answered();
        health.timed_out(health.mark());
        assert!(!fired(&health));
    }
}
