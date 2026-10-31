//! Versioned scalar lifecycle formats and bounded readers.
use super::{HeadRef, MAX_ROOT_BYTES, MAX_VALUE_BYTES, ObjectRef, PageRef, Root};
use crate::{
    Error, Result, corrupt,
    format::{self, Reader, Writer},
};
use obelisk_types::*;

pub const DESCRIPTOR_KIND: u8 = 8;
pub const INDEX_ENTRY_KIND: u8 = 9;
pub const PROJECTION_KIND: u8 = 10;
pub const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024;
pub const MAX_PROJECTION_BYTES: usize = MAX_DOCUMENT_BYTES + 2 * MAX_PATH_BYTES + 2048;
const DECODE_MAX: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootV3 {
    pub data: Root,
    pub catalog: Option<PageRef>,
    pub index_count: usize,
}
impl std::ops::Deref for RootV3 {
    type Target = Root;
    fn deref(&self) -> &Root {
        &self.data
    }
}
impl std::ops::DerefMut for RootV3 {
    fn deref_mut(&mut self) -> &mut Root {
        &mut self.data
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexGenerationRoot {
    pub generation: IndexGeneration,
    pub entries: Option<PageRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCandidate {
    pub tree: IndexGenerationRoot,
    pub base: HeadRef,
    pub progress: IndexBuildProgress,
    pub failure: Option<IndexBuildFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDescriptor {
    pub database_id: DatabaseId,
    pub id: IndexId,
    pub definition: ObjectRef,
    pub active: Option<IndexGenerationRoot>,
    pub candidate: Option<IndexCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub generation: IndexGeneration,
    pub location: format::Location,
    pub projection: Option<ObjectRef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CoveredProjection {
    pub generation: IndexGeneration,
    pub path: DocumentPath,
    pub revision: Revision,
    pub document: Document,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaggedOutcome {
    Document(format::RequestRecord),
    Index {
        fingerprint: [u8; 32],
        receipt: IndexReceipt,
    },
}
impl TaggedOutcome {
    pub fn fingerprint(&self) -> &[u8; 32] {
        match self {
            Self::Document(record) => &record.fingerprint,
            Self::Index { fingerprint, .. } => fingerprint,
        }
    }
    pub fn publication(&self) -> &CommitReceipt {
        match self {
            Self::Document(record) => &record.receipt,
            Self::Index { receipt, .. } => &receipt.publication,
        }
    }
}

pub(crate) fn decoded<T>(
    bytes: &[u8],
    kind: u8,
    version: u16,
    max: usize,
    budget: &mut usize,
    parse: impl FnOnce(&mut Reader<'_>) -> Result<T>,
) -> Result<T> {
    if bytes.len() > max {
        return Err(corrupt("index object byte limit"));
    }
    let payload = format::payload_version(bytes, kind, version)?;
    let initial = (*budget).min(DECODE_MAX);
    let mut reader = Reader::bounded(payload, initial);
    let result = parse(&mut reader);
    *budget -= initial - reader.remaining();
    let value = result.map_err(|error| match error {
        Error::Validation(error) => corrupt(&error.to_string()),
        other => other,
    })?;
    reader.finish()?;
    Ok(value)
}

fn path(reader: &mut Reader<'_>) -> Result<DocumentPath> {
    DocumentPath::from_segments(reader.path()?).map_err(|e| corrupt(&e.to_string()))
}
fn optional_page(writer: &mut Writer, page: &Option<PageRef>) {
    writer.byte(u8::from(page.is_some()));
    if let Some(page) = page {
        page.write(writer);
    }
}
fn read_page(reader: &mut Reader<'_>) -> Result<Option<PageRef>> {
    if reader.boolean()? {
        Ok(Some(PageRef::decode(reader)?))
    } else {
        Ok(None)
    }
}
fn optional_object(writer: &mut Writer, object: &Option<ObjectRef>) {
    writer.byte(u8::from(object.is_some()));
    if let Some(object) = object {
        object.write(writer);
    }
}
fn read_object(reader: &mut Reader<'_>, max: usize) -> Result<Option<ObjectRef>> {
    if reader.boolean()? {
        Ok(Some(ObjectRef::decode(reader, max)?))
    } else {
        Ok(None)
    }
}
fn generation(writer: &mut Writer, tree: &IndexGenerationRoot) {
    writer.text(tree.generation.as_str());
    optional_page(writer, &tree.entries);
}
fn read_generation(reader: &mut Reader<'_>) -> Result<IndexGenerationRoot> {
    Ok(IndexGenerationRoot {
        generation: IndexGeneration::new(reader.text(256)?)?,
        entries: read_page(reader)?,
    })
}
fn progress(writer: &mut Writer, progress: &IndexBuildProgress) {
    writer.u64(progress.examined());
    writer.byte(u8::from(progress.last_path().is_some()));
    if let Some(path) = progress.last_path() {
        writer.path(path.segments());
    }
}
fn read_progress(reader: &mut Reader<'_>) -> Result<IndexBuildProgress> {
    let examined = reader.u64()?;
    let last = if reader.boolean()? {
        Some(path(reader)?)
    } else {
        None
    };
    Ok(IndexBuildProgress::new(examined, last)?)
}
fn failure(writer: &mut Writer, failure: &Option<IndexBuildFailure>) {
    match failure {
        None => writer.byte(0),
        Some(IndexBuildFailure::ScalarKeyTooLarge(path)) => {
            writer.byte(1);
            writer.path(path.segments());
        }
    }
}
fn read_failure(reader: &mut Reader<'_>) -> Result<Option<IndexBuildFailure>> {
    match reader.byte()? {
        0 => Ok(None),
        1 => Ok(Some(IndexBuildFailure::ScalarKeyTooLarge(path(reader)?))),
        _ => Err(corrupt("unknown index build failure")),
    }
}
fn handle(writer: &mut Writer, handle: &IndexHandle) {
    writer.text(handle.id().as_str());
    writer.text(handle.generation().as_str());
}
fn read_handle(reader: &mut Reader<'_>) -> Result<IndexHandle> {
    Ok(IndexHandle::new(
        IndexId::new(reader.text(256)?)?,
        IndexGeneration::new(reader.text(256)?)?,
    ))
}
fn operation_tag(operation: IndexOperation) -> u8 {
    match operation {
        IndexOperation::Create => 0,
        IndexOperation::Advance => 1,
        IndexOperation::Rebuild => 2,
        IndexOperation::Drop => 3,
    }
}
fn read_operation(reader: &mut Reader<'_>) -> Result<IndexOperation> {
    match reader.byte()? {
        0 => Ok(IndexOperation::Create),
        1 => Ok(IndexOperation::Advance),
        2 => Ok(IndexOperation::Rebuild),
        3 => Ok(IndexOperation::Drop),
        _ => Err(corrupt("unknown index operation")),
    }
}

pub fn encode_root_v3(root: &RootV3) -> Result<Vec<u8>> {
    if root.index_count > MAX_LIVE_INDEXES {
        return Err(Error::ResourceLimit("live index count"));
    }
    let mut writer = Writer::default();
    writer.text(root.data.database_id.as_str());
    writer.text(root.data.commit_id.as_str());
    writer.u64(root.data.sequence);
    writer.u32(3); // Required: hierarchy membership and scalar index maintenance.
    for page in [
        &root.data.documents,
        &root.data.requests,
        &root.data.membership,
        &root.catalog,
    ] {
        optional_page(&mut writer, page);
    }
    writer.u32(root.index_count);
    let bytes = format::envelope_version(2, 3, writer.0)?;
    let mut validation_budget = DECODE_MAX;
    decode_root_v3(&bytes, &mut validation_budget)?;
    Ok(bytes)
}
pub fn decode_root_v3(bytes: &[u8], budget: &mut usize) -> Result<RootV3> {
    decoded(bytes, 2, 3, MAX_ROOT_BYTES, budget, |reader| {
        let database_id = DatabaseId::new(reader.text(256)?)?;
        let commit_id = CommitId::new(reader.text(256)?)?;
        let sequence = reader.u64()?;
        if reader.u32()? != 3 {
            return Err(corrupt("unsupported scalar root capabilities"));
        }
        let documents = read_page(reader)?;
        let requests = read_page(reader)?;
        let membership = read_page(reader)?;
        let catalog = read_page(reader)?;
        let index_count = reader.u32()?;
        if index_count > MAX_LIVE_INDEXES || (index_count == 0) != catalog.is_none() {
            return Err(corrupt("index catalog count/reference mismatch"));
        }
        Ok(RootV3 {
            data: Root {
                database_id,
                commit_id,
                sequence,
                documents,
                requests,
                membership,
            },
            catalog,
            index_count,
        })
    })
}
pub fn encode_head_v3(head: &HeadRef) -> Result<Vec<u8>> {
    let v2 = super::encode_head(head)?;
    format::envelope_version(1, 3, format::payload_version(&v2, 1, 2)?.to_vec())
}
pub fn decode_head_v3(bytes: &[u8], budget: &mut usize) -> Result<HeadRef> {
    decoded(
        bytes,
        1,
        3,
        obelisk_storage::MAX_HEAD_BYTES,
        budget,
        |reader| {
            Ok(HeadRef {
                database_id: DatabaseId::new(reader.text(256)?)?,
                commit_id: CommitId::new(reader.text(256)?)?,
                sequence: reader.u64()?,
                root: ObjectRef::decode(reader, MAX_ROOT_BYTES)?,
            })
        },
    )
}
pub fn encode_index_descriptor(descriptor: &IndexDescriptor) -> Result<Vec<u8>> {
    let mut writer = Writer::default();
    writer.text(descriptor.database_id.as_str());
    writer.text(descriptor.id.as_str());
    descriptor.definition.write(&mut writer);
    writer.byte(u8::from(descriptor.active.is_some()));
    if let Some(active) = &descriptor.active {
        generation(&mut writer, active);
    }
    writer.byte(u8::from(descriptor.candidate.is_some()));
    if let Some(candidate) = &descriptor.candidate {
        generation(&mut writer, &candidate.tree);
        writer.bytes(&encode_head_v3(&candidate.base)?);
        progress(&mut writer, &candidate.progress);
        failure(&mut writer, &candidate.failure);
    }
    let bytes = format::envelope(DESCRIPTOR_KIND, writer.0)?;
    let mut validation_budget = DECODE_MAX;
    decode_index_descriptor(&bytes, &mut validation_budget)?;
    Ok(bytes)
}
pub fn decode_index_descriptor(bytes: &[u8], budget: &mut usize) -> Result<IndexDescriptor> {
    decoded(
        bytes,
        DESCRIPTOR_KIND,
        1,
        MAX_DESCRIPTOR_BYTES,
        budget,
        |reader| {
            let database_id = DatabaseId::new(reader.text(256)?)?;
            let id = IndexId::new(reader.text(256)?)?;
            let definition = ObjectRef::decode(reader, MAX_VALUE_BYTES)?;
            let active = if reader.boolean()? {
                Some(read_generation(reader)?)
            } else {
                None
            };
            let candidate = if reader.boolean()? {
                let tree = read_generation(reader)?;
                // Decode directly with the same Reader budget; the nested head is bounded.
                let bytes = reader.bytes(obelisk_storage::MAX_HEAD_BYTES)?;
                let initial = reader.remaining();
                let mut remaining = initial;
                let result = decode_head_v3(bytes, &mut remaining);
                reader.charge(initial - remaining)?;
                let base = result?;
                Some(IndexCandidate {
                    tree,
                    base,
                    progress: read_progress(reader)?,
                    failure: read_failure(reader)?,
                })
            } else {
                None
            };
            if active.is_none() && candidate.is_none() {
                return Err(corrupt("index descriptor has no generation"));
            }
            if let Some(candidate) = &candidate
                && (candidate.base.database_id != database_id
                    || active
                        .as_ref()
                        .is_some_and(|active| active.generation == candidate.tree.generation))
            {
                return Err(corrupt("invalid candidate identity"));
            }
            Ok(IndexDescriptor {
                database_id,
                id,
                definition,
                active,
                candidate,
            })
        },
    )
}
pub fn encode_index_entry(entry: &IndexEntry) -> Result<Vec<u8>> {
    let mut writer = Writer::default();
    writer.text(entry.generation.as_str());
    writer.bytes(&super::encode_location(&entry.location)?);
    optional_object(&mut writer, &entry.projection);
    let bytes = format::envelope(INDEX_ENTRY_KIND, writer.0)?;
    let mut validation_budget = DECODE_MAX;
    decode_index_entry(&bytes, &mut validation_budget)?;
    Ok(bytes)
}
pub fn decode_index_entry(bytes: &[u8], budget: &mut usize) -> Result<IndexEntry> {
    decoded(
        bytes,
        INDEX_ENTRY_KIND,
        1,
        MAX_VALUE_BYTES,
        budget,
        |reader| {
            Ok(IndexEntry {
                generation: IndexGeneration::new(reader.text(256)?)?,
                location: super::decode_location(reader.bytes(MAX_VALUE_BYTES)?)?,
                projection: read_object(reader, MAX_PROJECTION_BYTES)?,
            })
        },
    )
}
pub fn encode_projection(projection: &CoveredProjection) -> Result<Vec<u8>> {
    projection.document.estimated_size()?;
    let mut writer = Writer::default();
    writer.text(projection.generation.as_str());
    writer.path(projection.path.segments());
    writer.text(projection.revision.as_str());
    writer.document(&projection.document);
    let bytes = format::envelope(PROJECTION_KIND, writer.0)?;
    let mut validation_budget = DECODE_MAX;
    decode_projection(&bytes, &mut validation_budget)?;
    Ok(bytes)
}
pub fn decode_projection(bytes: &[u8], budget: &mut usize) -> Result<CoveredProjection> {
    decoded(
        bytes,
        PROJECTION_KIND,
        1,
        MAX_PROJECTION_BYTES,
        budget,
        |reader| {
            Ok(CoveredProjection {
                generation: IndexGeneration::new(reader.text(256)?)?,
                path: path(reader)?,
                revision: Revision::new(reader.text(256)?)?,
                document: reader.document()?,
            })
        },
    )
}

pub fn index_command_fingerprint(command: &IndexCommand) -> Result<[u8; 32]> {
    let mut writer = Writer(b"ObeliskIndexOperation-v1\0".to_vec());
    writer.byte(operation_tag(command.operation()));
    match command {
        IndexCommand::Create(definition) => {
            writer.bytes(&super::encode_index_definition(definition)?)
        }
        IndexCommand::Advance(index, options) => {
            handle(&mut writer, index);
            writer.u32(options.documents());
            writer.u32(options.source_bytes());
        }
        IndexCommand::Rebuild(index) | IndexCommand::Drop(index) => handle(&mut writer, index),
    }
    Ok(*blake3::hash(&writer.0).as_bytes())
}

pub fn encode_tagged_outcome(outcome: &TaggedOutcome) -> Result<Vec<u8>> {
    let mut writer = Writer::default();
    writer.byte(u8::from(matches!(outcome, TaggedOutcome::Index { .. })));
    writer.0.extend_from_slice(outcome.fingerprint());
    format::write_receipt(&mut writer, outcome.publication());
    if let TaggedOutcome::Index { receipt, .. } = outcome {
        handle(&mut writer, &receipt.handle);
        writer.byte(operation_tag(receipt.operation));
        writer.byte(match receipt.state {
            IndexState::Building => 0,
            IndexState::Active => 1,
            IndexState::Failed => 2,
            IndexState::Dropped => 3,
        });
        progress(&mut writer, &receipt.progress);
        failure(&mut writer, &receipt.failure);
    }
    let bytes = format::envelope_version(super::OUTCOME_KIND, 2, writer.0)?;
    let mut validation_budget = DECODE_MAX;
    decode_tagged_outcome(&bytes, &mut validation_budget)?;
    Ok(bytes)
}
pub fn decode_tagged_outcome(bytes: &[u8], budget: &mut usize) -> Result<TaggedOutcome> {
    decoded(
        bytes,
        super::OUTCOME_KIND,
        2,
        super::MAX_OUTCOME_BYTES,
        budget,
        |reader| {
            let tag = reader.byte()?;
            let fingerprint = reader.take(32)?.try_into().unwrap();
            let publication = match tag {
                0 => format::read_receipt(reader)?,
                1 => {
                    // Management publications have no document revisions. Keep
                    // the original document receipt's nonempty check intact.
                    let database_id = DatabaseId::new(reader.text(256)?)?;
                    let request_id = RequestId::new(reader.text(256)?)?;
                    let commit_id = CommitId::new(reader.text(256)?)?;
                    let sequence = reader.u64()?;
                    if reader.u32()? != 0 {
                        return Err(corrupt("management receipt contains document revisions"));
                    }
                    CommitReceipt {
                        database_id,
                        request_id,
                        commit_id,
                        sequence,
                        revisions: std::collections::BTreeMap::new(),
                    }
                }
                _ => return Err(corrupt("unknown outcome operation kind")),
            };
            if publication.sequence == 0 {
                return Err(corrupt("invalid outcome sequence"));
            }
            if tag == 0 {
                return Ok(TaggedOutcome::Document(format::RequestRecord {
                    fingerprint,
                    receipt: publication,
                }));
            }
            let handle = read_handle(reader)?;
            let operation = read_operation(reader)?;
            let state = match reader.byte()? {
                0 => IndexState::Building,
                1 => IndexState::Active,
                2 => IndexState::Failed,
                3 => IndexState::Dropped,
                _ => return Err(corrupt("unknown index state")),
            };
            let progress = read_progress(reader)?;
            let failure = read_failure(reader)?;
            let valid_state = match operation {
                IndexOperation::Create | IndexOperation::Rebuild => {
                    state == IndexState::Building && progress.examined() == 0
                }
                IndexOperation::Advance => state != IndexState::Dropped,
                IndexOperation::Drop => state == IndexState::Dropped,
            };
            if !valid_state
                || !publication.revisions.is_empty()
                || (state == IndexState::Failed) != failure.is_some()
            {
                return Err(corrupt("invalid index outcome state"));
            }
            Ok(TaggedOutcome::Index {
                fingerprint,
                receipt: IndexReceipt {
                    publication,
                    handle,
                    operation,
                    state,
                    progress,
                    failure,
                },
            })
        },
    )
}

/// Validate a decoded candidate position against its separately stored definition.
pub fn validate_descriptor_definition(
    descriptor: &IndexDescriptor,
    definition: &IndexDefinition,
) -> Result<()> {
    if descriptor.id != *definition.id() {
        return Err(corrupt("index definition identity mismatch"));
    }
    if let Some(candidate) = &descriptor.candidate {
        if candidate
            .progress
            .last_path()
            .is_some_and(|path| !definition.scope().contains(path))
        {
            return Err(corrupt("index progress outside definition scope"));
        }
        if let Some(IndexBuildFailure::ScalarKeyTooLarge(path)) = &candidate.failure
            && !definition.scope().contains(path)
        {
            return Err(corrupt("index failure outside definition scope"));
        }
    }
    Ok(())
}
