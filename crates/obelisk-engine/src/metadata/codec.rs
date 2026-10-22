use super::*;
use crate::{
    corrupt,
    format::{self, Reader, Writer},
};
use obelisk_storage::{ObjectId, Storage};
use obelisk_types::{CommitId, DatabaseId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    pub object: ObjectId,
    pub len: u32,
    pub hash: [u8; 32],
}
impl ObjectRef {
    pub fn new(object: ObjectId, bytes: &[u8]) -> Self {
        Self {
            object,
            len: bytes.len() as u32,
            hash: *blake3::hash(bytes).as_bytes(),
        }
    }
    pub async fn read(
        &self,
        store: &dyn Storage,
        max: usize,
        budget: &mut Budget,
    ) -> Result<bytes::Bytes> {
        let len = self.len as usize;
        if !(format::HEADER_BYTES..=max).contains(&len) {
            return Err(corrupt("invalid referenced length"));
        }
        budget.charge_transfer(len)?;
        let bytes = store
            .read(self.object, Some(0..u64::from(self.len)))
            .await
            .map_err(|e| {
                if matches!(e, obelisk_storage::Error::NotFound) {
                    corrupt("missing referenced metadata")
                } else {
                    e.into()
                }
            })?;
        if bytes.len() != len || blake3::hash(&bytes).as_bytes() != &self.hash {
            return Err(corrupt("metadata reference hash/length mismatch"));
        }
        Ok(bytes)
    }
    pub(crate) fn write(&self, w: &mut Writer) {
        w.text(&self.object.to_string());
        w.u32(self.len as usize);
        w.0.extend_from_slice(&self.hash);
    }
    pub(crate) fn decode(r: &mut Reader<'_>, max: usize) -> Result<Self> {
        let object = r.object()?;
        let len = r.u32()?;
        if !(format::HEADER_BYTES..=max).contains(&len) {
            return Err(corrupt("invalid object reference length"));
        }
        Ok(Self {
            object,
            len: len as u32,
            hash: r.take(32)?.try_into().unwrap(),
        })
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRef {
    pub blob: ObjectRef,
    pub level: u8,
    pub count: u32,
}
impl PageRef {
    pub(crate) fn write(&self, w: &mut Writer) {
        self.blob.write(w);
        w.byte(self.level);
        w.u32(self.count as usize);
    }
    pub(crate) fn decode(r: &mut Reader<'_>) -> Result<Self> {
        let blob = ObjectRef::decode(r, MAX_PAGE_BYTES)?;
        let level = r.byte()?;
        let count = r.u32()?;
        if level >= MAX_HEIGHT || count == 0 || count > MAX_ENTRIES {
            return Err(corrupt("invalid page reference"));
        }
        Ok(Self {
            blob,
            level,
            count: count as u32,
        })
    }
    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut w = Writer::default();
        self.write(&mut w);
        w.0
    }
    pub(crate) fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let value = Self::decode(&mut r)?;
        r.finish()?;
        Ok(value)
    }
}
#[derive(Debug, Clone)]
pub struct Root {
    pub database_id: DatabaseId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub documents: Option<PageRef>,
    pub requests: Option<PageRef>,
    pub membership: Option<PageRef>,
}
pub fn encode_root(root: &Root) -> Result<Vec<u8>> {
    let mut w = Writer::default();
    w.text(root.database_id.as_str());
    w.text(root.commit_id.as_str());
    w.u64(root.sequence);
    w.u32(1); // Required capability: membership maintained from database creation.
    for page in [&root.documents, &root.requests, &root.membership] {
        w.byte(u8::from(page.is_some()));
        if let Some(page) = page {
            page.write(&mut w);
        }
    }
    let bytes = format::envelope_version(2, 2, w.0)?;
    if bytes.len() > MAX_ROOT_BYTES {
        return Err(Error::ResourceLimit("root manifest size"));
    }
    decode_root(&bytes)?;
    Ok(bytes)
}
pub fn decode_root(bytes: &[u8]) -> Result<Root> {
    if bytes.len() > MAX_ROOT_BYTES {
        return Err(corrupt("root manifest size"));
    }
    let mut r = Reader::new(format::payload_version(bytes, 2, 2)?);
    let database_id = DatabaseId::new(r.text(256)?).map_err(|e| corrupt(&e.to_string()))?;
    let commit_id = CommitId::new(r.text(256)?).map_err(|e| corrupt(&e.to_string()))?;
    let sequence = r.u64()?;
    if r.u32()? != 1 {
        return Err(corrupt("unsupported root capabilities"));
    }
    let mut page = || -> Result<_> {
        if r.boolean()? {
            Ok(Some(PageRef::decode(&mut r)?))
        } else {
            Ok(None)
        }
    };
    let documents = page()?;
    let requests = page()?;
    let membership = page()?;
    r.finish()?;
    Ok(Root {
        database_id,
        commit_id,
        sequence,
        documents,
        requests,
        membership,
    })
}
pub fn encode_outcome(record: &format::RequestRecord) -> Result<Vec<u8>> {
    let mut w = Writer::default();
    w.0.extend_from_slice(&record.fingerprint);
    format::write_receipt(&mut w, &record.receipt);
    let bytes = format::envelope(OUTCOME_KIND, w.0)?;
    if bytes.len() > MAX_OUTCOME_BYTES {
        return Err(Error::ResourceLimit("request outcome size"));
    }
    decode_outcome(&bytes)?;
    Ok(bytes)
}
pub fn decode_outcome(bytes: &[u8]) -> Result<format::RequestRecord> {
    if bytes.len() > MAX_OUTCOME_BYTES {
        return Err(corrupt("request outcome size"));
    }
    let mut r = Reader::new(format::payload(bytes, OUTCOME_KIND)?);
    let fingerprint = r.take(32)?.try_into().unwrap();
    let receipt = format::read_receipt(&mut r).map_err(|e| match e {
        Error::Validation(e) => corrupt(&e.to_string()),
        e => e,
    })?;
    if receipt.sequence == 0 {
        return Err(corrupt("invalid outcome sequence"));
    }
    r.finish()?;
    Ok(format::RequestRecord {
        fingerprint,
        receipt,
    })
}

pub(crate) type Entry = (Vec<u8>, Vec<u8>);
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub level: u8,
    pub entries: Vec<Entry>,
}
impl Node {
    pub fn size(&self) -> usize {
        format::HEADER_BYTES
            + 6
            + self
                .entries
                .iter()
                .map(|(k, v)| 8 + k.len() + v.len())
                .sum::<usize>()
    }
    pub fn encode(&self, family: Family) -> Result<Vec<u8>> {
        if self.size() > MAX_PAGE_BYTES {
            return Err(Error::ResourceLimit("metadata page size"));
        }
        let mut w = Writer::default();
        w.byte(family as u8);
        w.byte(self.level);
        w.u32(self.entries.len());
        for (k, v) in &self.entries {
            w.bytes(k);
            w.bytes(v);
        }
        let bytes = format::envelope(PAGE_KIND, w.0)?;
        Self::decode(&bytes, family)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8], family: Family) -> Result<Self> {
        if bytes.len() > MAX_PAGE_BYTES {
            return Err(corrupt("metadata page size"));
        }
        let mut r = Reader::new(format::payload(bytes, PAGE_KIND)?);
        if r.byte()? != family as u8 {
            return Err(corrupt("metadata family mismatch"));
        }
        let level = r.byte()?;
        if level >= MAX_HEIGHT {
            return Err(corrupt("metadata height limit"));
        }
        let count = r.count(MAX_ENTRIES, 64)?;
        if count == 0 {
            return Err(corrupt("empty metadata page"));
        }
        let mut entries = Vec::<Entry>::with_capacity(count);
        for _ in 0..count {
            let key = r.bytes(MAX_KEY_BYTES)?.to_vec();
            let value = r.bytes(MAX_VALUE_BYTES)?.to_vec();
            if key.is_empty() || entries.last().is_some_and(|e| e.0 >= key) {
                return Err(corrupt("metadata key order"));
            }
            if level > 0 && PageRef::from_bytes(&value)?.level + 1 != level {
                return Err(corrupt("metadata child level"));
            }
            entries.push((key, value));
        }
        r.finish()?;
        Ok(Self { level, entries })
    }
}
