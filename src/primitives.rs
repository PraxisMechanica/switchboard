//! Cross-cutting pure value primitives and generic combinators.

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest as Sha2Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

/// UTC timestamp in milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(i64);

impl Timestamp {
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

/// SHA-256 content digest used for request payload agreement and integrity checks.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn sha256(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let out = hasher.finalize();
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&out);
        Self(buf)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", &self.to_hex()[..16])
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for Digest {
    type Err = hex::FromHexError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s)?;
        if bytes.len() != 32 {
            return Err(hex::FromHexError::InvalidStringLength);
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        Ok(Self(out))
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Time source abstraction so domain services remain deterministic under test.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// Wall-clock implementation backed by [`SystemTime`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Timestamp(i64::try_from(millis).unwrap_or(i64::MAX))
    }
}

/// Deterministic clock for unit and integration tests.
#[derive(Debug, Clone)]
pub struct FixedClock(Arc<AtomicI64>);

impl FixedClock {
    pub fn new(millis: i64) -> Self {
        Self(Arc::new(AtomicI64::new(millis)))
    }

    pub fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.0.load(Ordering::SeqCst))
    }
}

/// Identifier generator abstraction for reproducible tests.
pub trait IdGen: Send + Sync {
    fn next_uuid(&self) -> Uuid;
}

/// Production UUIDv7 generator.
#[derive(Debug, Default, Clone, Copy)]
pub struct UuidV7Gen;

impl IdGen for UuidV7Gen {
    fn next_uuid(&self) -> Uuid {
        Uuid::now_v7()
    }
}

/// Stored outcome of an idempotent operation bound to its request fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptRecord<R> {
    pub request_hash: Digest,
    pub response: R,
}

/// Persistence contract for generic idempotent execution.
#[async_trait::async_trait]
pub trait ReceiptStore<K, R>: Send + Sync
where
    K: Send + Sync,
    R: Send + Sync,
{
    async fn load_receipt(&self, key: &K) -> Result<Option<ReceiptRecord<R>>, String>;

    async fn save_receipt(&self, key: &K, request_hash: Digest, response: &R)
        -> Result<(), String>;
}

/// Failure modes for [`run_idempotent`].
#[derive(Debug, Error)]
pub enum IdempotentError<E> {
    #[error("idempotency key reused with mismatched payload (expected {expected}, got {actual})")]
    PayloadMismatch { expected: Digest, actual: Digest },

    #[error("receipt store error: {0}")]
    Store(String),

    #[error(transparent)]
    Execution(E),
}

/// Executes an operation at most once per key, verifying payload agreement on replay.
pub async fn run_idempotent<K, R, E, S, F, Fut>(
    store: &S,
    key: &K,
    request_hash: Digest,
    execute: F,
) -> Result<R, IdempotentError<E>>
where
    K: Send + Sync,
    R: Send + Sync,
    S: ReceiptStore<K, R>,
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<R, E>>,
{
    if let Some(existing) = store
        .load_receipt(key)
        .await
        .map_err(IdempotentError::Store)?
    {
        if existing.request_hash != request_hash {
            return Err(IdempotentError::PayloadMismatch {
                expected: existing.request_hash,
                actual: request_hash,
            });
        }
        return Ok(existing.response);
    }

    let response = execute().await.map_err(IdempotentError::Execution)?;

    store
        .save_receipt(key, request_hash, &response)
        .await
        .map_err(IdempotentError::Store)?;

    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    struct MemReceipts(Mutex<HashMap<String, ReceiptRecord<String>>>);

    #[async_trait::async_trait]
    impl ReceiptStore<String, String> for MemReceipts {
        async fn load_receipt(
            &self,
            key: &String,
        ) -> Result<Option<ReceiptRecord<String>>, String> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        async fn save_receipt(
            &self,
            key: &String,
            request_hash: Digest,
            response: &String,
        ) -> Result<(), String> {
            self.0.lock().unwrap().insert(
                key.clone(),
                ReceiptRecord {
                    request_hash,
                    response: response.clone(),
                },
            );
            Ok(())
        }
    }

    #[tokio::test]
    async fn idempotent_replay_returns_cached_and_rejects_mismatch() {
        let store = MemReceipts(Mutex::new(HashMap::new()));
        let key = "msg-1".to_string();
        let h1 = Digest::sha256(b"payload-a");
        let h2 = Digest::sha256(b"payload-b");
        let calls = AtomicUsize::new(0);

        let first = run_idempotent::<_, _, String, _, _, _>(&store, &key, h1, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("answer-a".to_string())
        })
        .await
        .unwrap();
        assert_eq!(first, "answer-a");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let replay = run_idempotent::<_, _, String, _, _, _>(&store, &key, h1, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok("should-not-run".to_string())
        })
        .await
        .unwrap();
        assert_eq!(replay, "answer-a");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let mismatch = run_idempotent::<_, _, String, _, _, _>(&store, &key, h2, || async {
            Ok("should-not-run".to_string())
        })
        .await;
        assert!(matches!(
            mismatch,
            Err(IdempotentError::PayloadMismatch { .. })
        ));
    }
}
