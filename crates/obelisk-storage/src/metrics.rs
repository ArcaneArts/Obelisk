//! Logical adapter-operation counters. S3 may make several HTTP requests per
//! operation (and transport retries); these counters are not HTTP request counts.
use crate::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Default)]
struct Counters {
    head_reads: AtomicU64,
    object_reads: AtomicU64,
    range_reads: AtomicU64,
    bytes_read: AtomicU64,
    object_creates: AtomicU64,
    bytes_written: AtomicU64,
    head_cas_attempts: AtomicU64,
    head_cas_successes: AtomicU64,
    head_conflicts: AtomicU64,
    list_calls: AtomicU64,
    delete_calls: AtomicU64,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub head_reads: u64,
    pub object_reads: u64,
    pub range_reads: u64,
    pub bytes_read: u64,
    pub object_creates: u64,
    pub bytes_written: u64,
    pub head_cas_attempts: u64,
    pub head_cas_successes: u64,
    pub head_conflicts: u64,
    pub list_calls: u64,
    pub delete_calls: u64,
}
#[derive(Debug, Clone)]
pub struct InstrumentedStorage {
    inner: Arc<dyn Storage>,
    counters: Arc<Counters>,
}
impl InstrumentedStorage {
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        Self {
            inner,
            counters: Arc::default(),
        }
    }
    pub fn stats(&self) -> Stats {
        let c = &self.counters;
        Stats {
            head_reads: c.head_reads.load(Ordering::Relaxed),
            object_reads: c.object_reads.load(Ordering::Relaxed),
            range_reads: c.range_reads.load(Ordering::Relaxed),
            bytes_read: c.bytes_read.load(Ordering::Relaxed),
            object_creates: c.object_creates.load(Ordering::Relaxed),
            bytes_written: c.bytes_written.load(Ordering::Relaxed),
            head_cas_attempts: c.head_cas_attempts.load(Ordering::Relaxed),
            head_cas_successes: c.head_cas_successes.load(Ordering::Relaxed),
            head_conflicts: c.head_conflicts.load(Ordering::Relaxed),
            list_calls: c.list_calls.load(Ordering::Relaxed),
            delete_calls: c.delete_calls.load(Ordering::Relaxed),
        }
    }
}
#[async_trait]
impl Storage for InstrumentedStorage {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes> {
        self.counters.object_reads.fetch_add(1, Ordering::Relaxed);
        if range.is_some() {
            self.counters.range_reads.fetch_add(1, Ordering::Relaxed);
        }
        let bytes = self.inner.read(id, range).await?;
        self.counters
            .bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()> {
        self.counters.object_creates.fetch_add(1, Ordering::Relaxed);
        let len = bytes.len() as u64;
        self.inner.create(id, bytes).await?;
        self.counters
            .bytes_written
            .fetch_add(len, Ordering::Relaxed);
        Ok(())
    }
    async fn read_head(&self) -> Result<Option<Head>> {
        self.counters.head_reads.fetch_add(1, Ordering::Relaxed);
        let head = self.inner.read_head().await?;
        if let Some(h) = &head {
            self.counters
                .bytes_read
                .fetch_add(h.bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(head)
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken> {
        self.counters
            .head_cas_attempts
            .fetch_add(1, Ordering::Relaxed);
        let len = bytes.len() as u64;
        let result = self.inner.compare_exchange_head(expected, bytes).await;
        match &result {
            Ok(_) => {
                self.counters
                    .head_cas_successes
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .bytes_written
                    .fetch_add(len, Ordering::Relaxed);
            }
            Err(Error::Conflict) => {
                self.counters.head_conflicts.fetch_add(1, Ordering::Relaxed);
            }
            _ => (),
        }
        result
    }
    async fn list(&self) -> Result<Vec<ObjectId>> {
        self.counters.list_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.list().await
    }
    async fn delete(&self, id: ObjectId) -> Result<()> {
        self.counters.delete_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(id).await
    }
}
