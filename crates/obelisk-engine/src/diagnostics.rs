//! Optional logical object counters. These do not measure HTTP requests or RSS.
use bytes::Bytes;
use obelisk_storage::{Head, ObjectId, Result, Storage, VersionToken};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Clone, Copy)]
#[repr(usize)]
pub enum ObjectKind {
    Root,
    Documents,
    Requests,
    Membership,
    Outcome,
    Record,
    Segment,
    Unknown,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct ObjectStats {
    pub reads: u64,
    pub bytes_read: u64,
    pub creates: u64,
    pub bytes_written: u64,
    /// Page/root reader allocation charges inferred from the healthy envelope.
    /// Excludes editor copies, result clones, payload decoding and allocator RSS.
    pub reader_decode_charge: u64,
}
#[derive(Debug, Default)]
struct Counters {
    reads: AtomicU64,
    bytes_read: AtomicU64,
    creates: AtomicU64,
    bytes_written: AtomicU64,
    reader_decode_charge: AtomicU64,
}
#[derive(Debug)]
pub struct InstrumentedMetadata {
    inner: Arc<dyn Storage>,
    counts: [Counters; 8],
}
fn kind(bytes: &[u8]) -> ObjectKind {
    if bytes.len() < 43 || &bytes[..4] != b"OBLS" {
        return ObjectKind::Unknown;
    }
    match bytes[4] {
        2 => ObjectKind::Root,
        3 => ObjectKind::Segment,
        4 => ObjectKind::Record,
        6 => ObjectKind::Outcome,
        5 => match bytes.get(43) {
            Some(1) => ObjectKind::Documents,
            Some(2) => ObjectKind::Requests,
            Some(3) => ObjectKind::Membership,
            _ => ObjectKind::Unknown,
        },
        _ => ObjectKind::Unknown,
    }
}
impl InstrumentedMetadata {
    pub fn new(inner: Arc<dyn Storage>) -> Self {
        Self {
            inner,
            counts: std::array::from_fn(|_| Counters::default()),
        }
    }
    pub fn stats(&self, kind: ObjectKind) -> ObjectStats {
        let c = &self.counts[kind as usize];
        ObjectStats {
            reads: c.reads.load(Ordering::Relaxed),
            bytes_read: c.bytes_read.load(Ordering::Relaxed),
            creates: c.creates.load(Ordering::Relaxed),
            bytes_written: c.bytes_written.load(Ordering::Relaxed),
            reader_decode_charge: c.reader_decode_charge.load(Ordering::Relaxed),
        }
    }
}
#[async_trait::async_trait]
impl Storage for InstrumentedMetadata {
    async fn read(&self, id: ObjectId, range: Option<Range<u64>>) -> Result<Bytes> {
        let bytes = self.inner.read(id, range).await?;
        let kind = kind(&bytes);
        let c = &self.counts[kind as usize];
        c.reads.fetch_add(1, Ordering::Relaxed);
        c.bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let charge = match kind {
            ObjectKind::Root => bytes.len() as u64 * 2 + 512,
            ObjectKind::Documents | ObjectKind::Requests | ObjectKind::Membership => bytes
                .get(45..49)
                .map(|s| {
                    bytes.len() as u64 * 2
                        + u64::from(u32::from_le_bytes(s.try_into().unwrap())) * 64
                })
                .unwrap_or(0),
            _ => 0,
        };
        c.reader_decode_charge.fetch_add(charge, Ordering::Relaxed);
        Ok(bytes)
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> Result<()> {
        let kind = kind(&bytes);
        let len = bytes.len() as u64;
        self.inner.create(id, bytes).await?;
        let c = &self.counts[kind as usize];
        c.creates.fetch_add(1, Ordering::Relaxed);
        c.bytes_written.fetch_add(len, Ordering::Relaxed);
        Ok(())
    }
    async fn read_head(&self) -> Result<Option<Head>> {
        self.inner.read_head().await
    }
    async fn compare_exchange_head(
        &self,
        v: Option<&VersionToken>,
        bytes: Bytes,
    ) -> Result<VersionToken> {
        self.inner.compare_exchange_head(v, bytes).await
    }
    async fn list(&self) -> Result<Vec<ObjectId>> {
        self.inner.list().await
    }
    async fn delete(&self, id: ObjectId) -> Result<()> {
        self.inner.delete(id).await
    }
}
