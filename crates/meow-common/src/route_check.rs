//! Publication fence for controller route diagnostics. The daemon's origin
//! resolver and outbound settings are process-wide, as is this fence.

use std::future::Future;
use std::sync::LazyLock;
use tokio::sync::watch;

#[derive(Clone, Copy, Default)]
pub struct Revision {
    number: u64,
    publishers: usize,
}

impl Revision {
    /// A revision can be used only after every overlapping install finishes.
    pub fn current(self) -> Option<u64> {
        (self.publishers == 0).then_some(self.number)
    }
}

static REVISION: LazyLock<watch::Sender<Revision>> =
    LazyLock::new(|| watch::Sender::new(Revision::default()));

pub fn subscribe() -> watch::Receiver<Revision> {
    REVISION.subscribe()
}

/// Keep alive from before the first live write until after the last one.
/// Installers may nest and must enter before acquiring their own locks.
#[derive(Debug)]
#[must_use]
pub struct Publication(());

pub fn publishing() -> Publication {
    REVISION.send_modify(|revision| {
        revision.number = revision
            .number
            .checked_add(1)
            .expect("route revision exhausted");
        revision.publishers += 1;
    });
    Publication(())
}

impl Drop for Publication {
    fn drop(&mut self) {
        REVISION.send_modify(|revision| revision.publishers -= 1);
    }
}

tokio::task_local! {
    static ROUTE_CHECK: ();
}

pub async fn scope<F: Future>(future: F) -> F::Output {
    ROUTE_CHECK.scope((), future).await
}

pub fn active() -> bool {
    ROUTE_CHECK.try_with(|()| ()).is_ok()
}
