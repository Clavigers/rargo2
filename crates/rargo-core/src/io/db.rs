//! Lifecycle of the background database writer on the I/O loop.

use super::{Notify, background, loop_running, timeout};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

type WakeSlot = OnceLock<Arc<Notify>>;

#[derive(Default)]
pub(crate) struct WriterTask {
    slot: OnceLock<Arc<WakeSlot>>,
}

impl WriterTask {
    /// A task that never starts: writes wait for an explicit flush.
    #[cfg(test)]
    pub(crate) fn off() -> WriterTask {
        let slot = OnceLock::new();
        assert!(slot.set(Arc::new(WakeSlot::new())).is_ok());
        WriterTask { slot }
    }

    pub(crate) fn written(&self, run: impl FnOnce(WriterWake) + Send + 'static) {
        if self.slot.get().is_none() && !loop_running() {
            return;
        }
        let slot = self.slot.get_or_init(|| {
            let slot = Arc::new(WakeSlot::new());
            let owner = Arc::downgrade(&slot);
            background(move || {
                let Some(slot) = owner.upgrade() else { return };
                let wake = Arc::new(Notify::new());
                assert!(slot.set(Arc::clone(&wake)).is_ok());
                drop(slot);
                run(WriterWake { owner, wake });
            });
            slot
        });
        if let Some(wake) = slot.get() {
            wake.notify_one();
        }
    }

    pub(crate) fn stop(&mut self) {
        // Release ownership before waking the task so its next step sees shutdown.
        let wake = self.slot.take().and_then(|slot| slot.get().cloned());
        if let Some(wake) = wake {
            wake.notify_one();
        }
    }
}

pub(crate) struct WriterWake {
    owner: Weak<WakeSlot>,
    wake: Arc<Notify>,
}

impl WriterWake {
    pub(crate) fn running(&self) -> bool {
        self.owner.upgrade().is_some()
    }

    pub(crate) fn wait(&self, duration: Duration) {
        let _ = timeout(duration, || self.wake.notified());
    }
}
