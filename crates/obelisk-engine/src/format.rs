//! Explicit prototype format v1. Not derived from public Rust data structures.
//! All integers are little endian. Each object/record is kind/version/checksummed.
use crate::{Error, Result, corrupt};
use obelisk_storage::{MAX_OBJECT_BYTES, ObjectId};
use obelisk_types::*;
use std::{collections::BTreeMap, ops::Range};

pub const HEADER_BYTES: usize = 43;
const MAX_PAYLOAD: usize = 64 * 1024 * 1024;
const MAX_DECODE_BUDGET: usize = 128 * 1024 * 1024;
const HEAD: u8 = 1;
const MANIFEST: u8 = 2;
const SEGMENT: u8 = 3;
const RECORD: u8 = 4;

#[derive(Debug, Clone)]
pub struct HeadRef {
    pub database_id: DatabaseId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub manifest: ObjectId,
    pub manifest_hash: [u8; 32],
}
#[derive(Debug, Clone)]
pub struct Location {
    pub object: ObjectId,
    pub range: Range<u64>,
    pub revision: Revision,
}
#[derive(Debug, Clone)]
pub struct Manifest {
    pub database_id: DatabaseId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub latest_segment: Option<ObjectId>,
    pub documents: BTreeMap<DocumentPath, Location>,
    pub requests: BTreeMap<RequestId, RequestRecord>,
}
#[derive(Debug, Clone)]
pub struct RequestRecord {
    pub fingerprint: [u8; 32],
    pub receipt: CommitReceipt,
}
fn envelope(kind: u8, payload: Vec<u8>) -> Result<Vec<u8>> {
    if payload.len() > MAX_PAYLOAD {
        return Err(corrupt("format payload exceeds limit"));
    }
    let mut out = b"OBLS".to_vec();
    out.push(kind);
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(blake3::hash(&payload).as_bytes());
    out.extend(payload);
    Ok(out)
}
fn payload(bytes: &[u8], kind: u8) -> Result<&[u8]> {
    if bytes.len() < HEADER_BYTES || &bytes[..4] != b"OBLS" || bytes[4] != kind {
        return Err(corrupt("invalid envelope"));
    }
    let version = u16::from_le_bytes(bytes[5..7].try_into().unwrap());
    if version != 1 {
        return Err(Error::UnsupportedVersion(version));
    }
    let len = u32::from_le_bytes(bytes[7..11].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD || bytes.len() != HEADER_BYTES + len {
        return Err(corrupt("invalid envelope length"));
    }
    let data = &bytes[HEADER_BYTES..];
    if blake3::hash(data).as_bytes() != &bytes[11..43] {
        return Err(corrupt("checksum mismatch"));
    }
    Ok(data)
}
#[derive(Default)]
pub(crate) struct Writer(pub Vec<u8>);
impl Writer {
    pub(crate) fn byte(&mut self, x: u8) {
        self.0.push(x);
    }
    pub(crate) fn u32(&mut self, x: usize) {
        self.0.extend_from_slice(&(x as u32).to_le_bytes());
    }
    pub(crate) fn u64(&mut self, x: u64) {
        self.0.extend_from_slice(&x.to_le_bytes());
    }
    pub(crate) fn bytes(&mut self, x: &[u8]) {
        self.u32(x.len());
        self.0.extend_from_slice(x);
    }
    pub(crate) fn text(&mut self, x: &str) {
        self.bytes(x.as_bytes());
    }
    pub(crate) fn path(&mut self, x: &[String]) {
        self.u32(x.len());
        for s in x {
            self.text(s);
        }
    }
    fn value_type(&mut self, t: &ValueType) {
        self.byte(match t {
            ValueType::Binary => 0,
            ValueType::String => 1,
            ValueType::Integer => 2,
            ValueType::Double => 3,
            ValueType::Boolean => 4,
            ValueType::List(_) => 5,
        });
        if let ValueType::List(t) = t {
            self.value_type(t);
        }
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::Binary(x) => {
                self.byte(0);
                self.bytes(x);
            }
            Value::String(x) => {
                self.byte(1);
                self.text(x);
            }
            Value::Integer(x) => {
                self.byte(2);
                self.u64(*x as u64);
            }
            Value::Double(x) => {
                self.byte(3);
                self.u64(x.get().to_bits());
            }
            Value::Boolean(x) => {
                self.byte(4);
                self.byte(u8::from(*x));
            }
            Value::List(x) => {
                self.byte(5);
                self.value_type(x.element_type());
                self.u32(x.values().len());
                for v in x.values() {
                    self.value(v);
                }
            }
        }
    }
    pub(crate) fn document(&mut self, d: &Document) {
        self.u32(d.fields().len());
        for (p, v) in d.fields() {
            self.path(p.segments());
            self.value(v);
        }
    }
}
struct Reader<'a> {
    input: &'a [u8],
    budget: usize,
}
impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            budget: MAX_DECODE_BUDGET,
        }
    }
    fn charge(&mut self, n: usize) -> Result<()> {
        self.budget = self
            .budget
            .checked_sub(n)
            .ok_or_else(|| corrupt("decode allocation budget"))?;
        Ok(())
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.input.len() {
            return Err(corrupt("truncated payload"));
        }
        let (v, rest) = self.input.split_at(n);
        self.input = rest;
        Ok(v)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn boolean(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(corrupt("invalid boolean")),
        }
    }
    fn u32(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()) as usize)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn count(&mut self, max: usize, cost: usize) -> Result<usize> {
        let n = self.u32()?;
        if n > max || n > self.input.len() {
            return Err(corrupt("invalid item count"));
        }
        self.charge(
            n.checked_mul(cost)
                .ok_or_else(|| corrupt("allocation overflow"))?,
        )?;
        Ok(n)
    }
    fn bytes(&mut self, max: usize) -> Result<&'a [u8]> {
        let n = self.u32()?;
        if n > max {
            return Err(corrupt("byte limit"));
        }
        self.charge(n)?;
        self.take(n)
    }
    fn text(&mut self, max: usize) -> Result<String> {
        std::str::from_utf8(self.bytes(max)?)
            .map(str::to_owned)
            .map_err(|_| corrupt("invalid UTF-8"))
    }
    fn path(&mut self) -> Result<Vec<String>> {
        let n = self.count(128, 32)?;
        let mut parts = Vec::with_capacity(n);
        for _ in 0..n {
            parts.push(self.text(MAX_PATH_BYTES)?);
        }
        Ok(parts)
    }
    fn object(&mut self) -> Result<ObjectId> {
        ObjectId::parse(&self.text(36)?).map_err(|_| corrupt("invalid object identity"))
    }
    fn value_type(&mut self, depth: usize) -> Result<ValueType> {
        if depth > MAX_DEPTH {
            return Err(corrupt("type depth limit"));
        }
        match self.byte()? {
            0 => Ok(ValueType::Binary),
            1 => Ok(ValueType::String),
            2 => Ok(ValueType::Integer),
            3 => Ok(ValueType::Double),
            4 => Ok(ValueType::Boolean),
            5 => Ok(ValueType::List(Box::new(self.value_type(depth + 1)?))),
            _ => Err(corrupt("unknown value type")),
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(corrupt("value depth limit"));
        }
        Ok(match self.byte()? {
            0 => Value::Binary(self.bytes(MAX_DOCUMENT_BYTES)?.to_vec()),
            1 => Value::String(self.text(MAX_DOCUMENT_BYTES)?),
            2 => Value::Integer(self.u64()? as i64),
            3 => Value::double(f64::from_bits(self.u64()?))?,
            4 => Value::Boolean(self.boolean()?),
            5 => {
                let t = self.value_type(depth + 1)?;
                let n = self.count(MAX_LIST_ITEMS, 64)?;
                let mut values = Vec::with_capacity(n);
                for _ in 0..n {
                    values.push(self.value(depth + 1)?);
                }
                Value::List(TypedList::new(t, values)?)
            }
            _ => return Err(corrupt("unknown value tag")),
        })
    }
    fn document(&mut self) -> Result<Document> {
        let n = self.count(MAX_FIELDS, 128)?;
        let mut fields = Vec::with_capacity(n);
        for _ in 0..n {
            fields.push((FieldPath::from_segments(self.path()?)?, self.value(0)?));
        }
        Ok(Document::new(fields)?)
    }
    fn finish(self) -> Result<()> {
        if self.input.is_empty() {
            Ok(())
        } else {
            Err(corrupt("trailing bytes"))
        }
    }
}
pub fn encode_head(head: &HeadRef) -> Result<Vec<u8>> {
    let mut w = Writer::default();
    w.text(head.database_id.as_str());
    w.text(head.commit_id.as_str());
    w.u64(head.sequence);
    w.text(&head.manifest.to_string());
    w.0.extend_from_slice(&head.manifest_hash);
    envelope(HEAD, w.0)
}
pub fn decode_head(bytes: &[u8]) -> Result<HeadRef> {
    let mut r = Reader::new(payload(bytes, HEAD)?);
    let head = HeadRef {
        database_id: DatabaseId::new(r.text(256)?)?,
        commit_id: CommitId::new(r.text(256)?)?,
        sequence: r.u64()?,
        manifest: r.object()?,
        manifest_hash: r.take(32)?.try_into().unwrap(),
    };
    r.finish()?;
    Ok(head)
}
pub fn encode_manifest(m: &Manifest) -> Result<Vec<u8>> {
    let mut w = Writer::default();
    w.text(m.database_id.as_str());
    w.text(m.commit_id.as_str());
    w.u64(m.sequence);
    w.byte(u8::from(m.latest_segment.is_some()));
    if let Some(s) = m.latest_segment {
        w.text(&s.to_string());
    }
    w.u32(m.documents.len());
    for (p, l) in &m.documents {
        w.path(p.segments());
        w.text(&l.object.to_string());
        w.u64(l.range.start);
        w.u64(l.range.end);
        w.text(l.revision.as_str());
        if w.0.len() > MAX_PAYLOAD {
            return Err(corrupt("metadata limit"));
        }
    }
    w.u32(m.requests.len());
    for (id, record) in &m.requests {
        w.text(id.as_str());
        w.0.extend_from_slice(&record.fingerprint);
        write_receipt(&mut w, &record.receipt);
        if w.0.len() > MAX_PAYLOAD {
            return Err(corrupt("request metadata limit"));
        }
    }
    envelope(MANIFEST, w.0)
}
pub fn decode_manifest(bytes: &[u8]) -> Result<Manifest> {
    let mut r = Reader::new(payload(bytes, MANIFEST)?);
    let database_id = DatabaseId::new(r.text(256)?)?;
    let commit_id = CommitId::new(r.text(256)?)?;
    let sequence = r.u64()?;
    let latest_segment = if r.boolean()? {
        Some(r.object()?)
    } else {
        None
    };
    let count = r.count(1_000_000, 256)?;
    let mut documents = BTreeMap::new();
    for _ in 0..count {
        let p = DocumentPath::from_segments(r.path()?)?;
        let l = Location {
            object: r.object()?,
            range: r.u64()?..r.u64()?,
            revision: Revision::new(r.text(256)?)?,
        };
        if l.range.start < HEADER_BYTES as u64
            || l.range.end <= l.range.start
            || l.range.end > MAX_OBJECT_BYTES as u64
            || l.range.end - l.range.start > (MAX_DOCUMENT_BYTES + MAX_PATH_BYTES + 1024) as u64
        {
            return Err(corrupt("invalid document location"));
        }
        if documents.insert(p, l).is_some() {
            return Err(corrupt("duplicate document location"));
        }
    }
    let count = r.count(1_000_000, 256)?;
    let mut requests = BTreeMap::new();
    for _ in 0..count {
        let id = RequestId::new(r.text(256)?)?;
        let fingerprint = r.take(32)?.try_into().unwrap();
        let receipt = read_receipt(&mut r)?;
        if receipt.request_id != id
            || receipt.database_id != database_id
            || receipt.sequence == 0
            || receipt.sequence > sequence
        {
            return Err(corrupt("invalid request receipt identity"));
        }
        if requests
            .insert(
                id,
                RequestRecord {
                    fingerprint,
                    receipt,
                },
            )
            .is_some()
        {
            return Err(corrupt("duplicate request identity"));
        }
    }
    r.finish()?;
    Ok(Manifest {
        database_id,
        commit_id,
        sequence,
        latest_segment,
        documents,
        requests,
    })
}
fn write_receipt(w: &mut Writer, receipt: &CommitReceipt) {
    w.text(receipt.database_id.as_str());
    w.text(receipt.request_id.as_str());
    w.text(receipt.commit_id.as_str());
    w.u64(receipt.sequence);
    w.u32(receipt.revisions.len());
    for (path, revision) in &receipt.revisions {
        w.path(path.segments());
        w.byte(u8::from(revision.is_some()));
        if let Some(revision) = revision {
            w.text(revision.as_str());
        }
    }
}
fn read_receipt(r: &mut Reader<'_>) -> Result<CommitReceipt> {
    let database_id = DatabaseId::new(r.text(256)?)?;
    let request_id = RequestId::new(r.text(256)?)?;
    let commit_id = CommitId::new(r.text(256)?)?;
    let sequence = r.u64()?;
    let count = r.count(MAX_BATCH_ITEMS, 128)?;
    let mut revisions = BTreeMap::new();
    for _ in 0..count {
        let path = DocumentPath::from_segments(r.path()?)?;
        let revision = if r.boolean()? {
            Some(Revision::new(r.text(256)?)?)
        } else {
            None
        };
        if revision
            .as_ref()
            .is_some_and(|v| v.as_str() != commit_id.as_str())
        {
            return Err(corrupt("receipt revision identity"));
        }
        if revisions.insert(path, revision).is_some() {
            return Err(corrupt("duplicate receipt path"));
        }
    }
    if revisions.is_empty() {
        return Err(corrupt("empty commit receipt"));
    }
    Ok(CommitReceipt {
        database_id,
        request_id,
        commit_id,
        sequence,
        revisions,
    })
}
/// Canonical operation encoding: independent mutation/condition order is normalized.
/// Request identity is excluded, so the digest describes only the operation.
pub fn transaction_fingerprint(transaction: &Transaction) -> Result<[u8; 32]> {
    fn condition(w: &mut Writer, c: &Condition) {
        match c {
            Condition::Any => w.byte(0),
            Condition::Absent => w.byte(1),
            Condition::Revision(v) => {
                w.byte(2);
                w.text(v.as_str());
            }
        }
    }
    let mut w = Writer(b"Obelisk transaction v1".to_vec());
    let mut mutations: Vec<_> = transaction.mutations().iter().collect();
    mutations.sort_by(|a, b| a.path.cmp(&b.path));
    w.u32(mutations.len());
    for mutation in mutations {
        w.path(mutation.path.segments());
        condition(&mut w, &mutation.condition);
        match &mutation.kind {
            MutationKind::Put(d) => {
                w.byte(0);
                w.document(d);
            }
            MutationKind::Patch(p) => {
                w.byte(1);
                w.document(&Document::new(p.set().clone())?);
                w.u32(p.remove().len());
                for path in p.remove() {
                    w.path(path.segments());
                }
            }
            MutationKind::Delete => w.byte(2),
        }
    }
    let mut conditions = Vec::new();
    for c in transaction.conditions() {
        let mut item = Writer::default();
        item.path(c.path.segments());
        condition(&mut item, &c.condition);
        conditions.push(item.0);
    }
    conditions.sort();
    w.u32(conditions.len());
    for item in conditions {
        w.bytes(&item);
    }
    Ok(*blake3::hash(&w.0).as_bytes())
}
pub fn encode_record(path: &DocumentPath, document: Option<&Document>) -> Result<Vec<u8>> {
    let mut w = Writer::default();
    w.path(path.segments());
    w.byte(u8::from(document.is_some()));
    if let Some(d) = document {
        w.document(d);
    }
    envelope(RECORD, w.0)
}
pub fn decode_record(bytes: &[u8]) -> Result<(DocumentPath, Option<Document>)> {
    let mut r = Reader::new(payload(bytes, RECORD)?);
    let p = DocumentPath::from_segments(r.path()?)?;
    let d = if r.boolean()? {
        Some(r.document()?)
    } else {
        None
    };
    r.finish()?;
    Ok((p, d))
}
pub type SegmentLocations = BTreeMap<DocumentPath, Range<u64>>;
/// A sorted immutable batch of independently checksummed document/tombstone records.
pub fn encode_segment(
    records: &BTreeMap<DocumentPath, Option<Document>>,
) -> Result<(Vec<u8>, SegmentLocations)> {
    let mut w = Writer::default();
    w.u32(records.len());
    let mut locations = BTreeMap::new();
    for (path, document) in records {
        let bytes = encode_record(path, document.as_ref())?;
        let start = (HEADER_BYTES + w.0.len()) as u64;
        w.0.extend_from_slice(&bytes);
        locations.insert(path.clone(), start..(HEADER_BYTES + w.0.len()) as u64);
    }
    Ok((envelope(SEGMENT, w.0)?, locations))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn all_value_types_roundtrip_and_corruption_fails() {
        let p = DocumentPath::from_segments(["a/b", "c"]).unwrap();
        let values = vec![
            Value::Binary(vec![0, 255]),
            Value::String("héllo".into()),
            Value::Integer(i64::MIN),
            Value::double(-0.0).unwrap(),
            Value::Boolean(false),
            Value::List(TypedList::new(ValueType::Integer, vec![]).unwrap()),
            Value::List(
                TypedList::new(
                    ValueType::List(Box::new(ValueType::Boolean)),
                    vec![Value::List(
                        TypedList::new(ValueType::Boolean, vec![Value::Boolean(true)]).unwrap(),
                    )],
                )
                .unwrap(),
            ),
        ];
        let d = Document::new(
            values
                .into_iter()
                .enumerate()
                .map(|(i, v)| (FieldPath::from_segments([i.to_string()]).unwrap(), v)),
        )
        .unwrap();
        let encoded = encode_record(&p, Some(&d)).unwrap();
        assert_eq!(
            decode_record(&encoded).unwrap(),
            (p.clone(), Some(d.clone()))
        );
        for index in [0, 4, 7, 11, encoded.len() - 1] {
            let mut bad = encoded.clone();
            bad[index] ^= 1;
            assert!(decode_record(&bad).is_err());
        }
        let mut future = encoded.clone();
        future[5..7].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            decode_record(&future),
            Err(Error::UnsupportedVersion(2))
        ));
        let (segment, locations) =
            encode_segment(&BTreeMap::from([(p.clone(), Some(d.clone()))])).unwrap();
        let range = locations[&p].clone();
        assert_eq!(
            decode_record(&segment[range.start as usize..range.end as usize]).unwrap(),
            (p, Some(d))
        );
    }
    #[test]
    fn hostile_lengths_and_trailing_data_are_bounded() {
        let bytes = envelope(RECORD, vec![255; 32]).unwrap();
        assert!(decode_record(&bytes).is_err());
        let mut bytes = encode_record(&DocumentPath::parse("x").unwrap(), None).unwrap();
        bytes.push(0);
        assert!(decode_record(&bytes).is_err());
    }
    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(),0..2048)) {
            let _=decode_record(&bytes);let _=decode_manifest(&bytes);let _=decode_head(&bytes);
        }
        #[test]
        fn scalar_document_roundtrip(i in any::<i64>(), text in ".{0,100}", binary in prop::collection::vec(any::<u8>(),0..100)) {
            let p=DocumentPath::parse("test").unwrap();
            let d=Document::new([(FieldPath::parse("i").unwrap(),Value::Integer(i)),(FieldPath::parse("s").unwrap(),Value::String(text)),(FieldPath::parse("b").unwrap(),Value::Binary(binary))]).unwrap();
            prop_assert_eq!(decode_record(&encode_record(&p,Some(&d)).unwrap()).unwrap(),(p,Some(d)));
        }
    }
}
