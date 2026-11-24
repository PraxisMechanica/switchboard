//! Per-`CallId` async turn serializer preventing concurrent access to one resumed harness thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use super::model::CallId;

#[derive(Clone, Default)]
pub struct CallTurnLocks {
    locks: Arc<Mutex<HashMap<CallId, Arc<AsyncMutex<()>>>>>,
}

impl CallTurnLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn acquire(&self, call_id: CallId) -> OwnedMutexGuard<()> {
        let lock = {
            let mut guard = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(
                guard
                    .entry(call_id)
                    .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
            )
        };
        lock.lock_owned().await
    }
}
