//! Benchmark-only accounting: each wrapper belongs to one sequential writer.
//! Creates are charged to the following CAS outcome, without mixing writers.
use bytes::Bytes;
use obelisk::storage::{Error, Head, ObjectId, Result, Storage, VersionToken};
use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

#[derive(Debug, Default, Clone, Copy)]
pub struct Uploads {
    pub attempts: u64,
    pub objects: u64,
    pub bytes: u64,
}
impl Uploads {
    fn add(&mut self, other: Self) {
        self.attempts += other.attempts;
        self.objects += other.objects;
        self.bytes += other.bytes;
    }
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Accounting {
    pub committed: Uploads,
    pub abandoned: Uploads,
    pub unclassified: Uploads,
    pub pending: Uploads,
}
impl Accounting {
    pub fn add(&mut self, other: Self) {
        self.committed.add(other.committed);
        self.abandoned.add(other.abandoned);
        self.unclassified.add(other.unclassified);
        self.pending.add(other.pending);
    }
}
#[derive(Debug)]
pub struct Publications {
    inner: Arc<dyn Storage>,
    accounting: Mutex<Accounting>,
}
impl Publications {
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        Self {
            inner,
            accounting: Mutex::default(),
        }
    }
    pub fn stats(&self) -> Accounting {
        *self.accounting.lock().unwrap()
    }
}
#[async_trait::async_trait]
impl Storage for Publications {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes> {
        self.inner.read(id, range).await
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()> {
        let len = bytes.len() as u64;
        self.inner.create(id, bytes).await?;
        let mut accounting = self.accounting.lock().unwrap();
        accounting.pending.objects += 1;
        accounting.pending.bytes += len;
        Ok(())
    }
    async fn read_head(&self) -> Result<Option<Head>> {
        self.inner.read_head().await
    }
    async fn compare_exchange_head(
        &self,
        expected: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken> {
        let result = self.inner.compare_exchange_head(expected, bytes).await;
        let mut accounting = self.accounting.lock().unwrap();
        let mut uploads = std::mem::take(&mut accounting.pending);
        uploads.attempts = 1;
        match &result {
            Ok(_) => accounting.committed.add(uploads),
            Err(Error::Conflict) => accounting.abandoned.add(uploads),
            _ => accounting.unclassified.add(uploads),
        }
        result
    }
    async fn list(&self) -> Result<Vec<ObjectId>> {
        self.inner.list().await
    }
    async fn delete(&self, id: ObjectId) -> Result<()> {
        self.inner.delete(id).await
    }
}
