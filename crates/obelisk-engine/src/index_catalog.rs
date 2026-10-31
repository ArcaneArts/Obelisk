//! Bounded catalog loading and preparation shared by lifecycle and writers.
use crate::{
    Error, Result, Snapshot, corrupt, format,
    metadata::{
        self, Budget, Family, KeyRange, ObjectRef,
        lifecycle::{self, IndexDescriptor, RootV3},
    },
};
use bytes::Bytes;
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::ops::Bound;

pub(crate) type Objects = Vec<(ObjectId, Bytes)>;
pub(crate) struct LoadedIndex {
    pub descriptor: IndexDescriptor,
    pub definition: IndexDefinition,
}
impl LoadedIndex {
    pub fn maintained(&self) -> bool {
        self.descriptor.active.is_some()
            || self
                .descriptor
                .candidate
                .as_ref()
                .is_some_and(|c| c.failure.is_none())
    }
    pub fn status(self, budget: &mut Budget) -> Result<IndexStatus> {
        budget.charge_decoded(2048)?;
        let id = self.definition.id();
        let active = self
            .descriptor
            .active
            .map(|tree| IndexHandle::new(id.clone(), tree.generation));
        let candidate = self.descriptor.candidate.map(|candidate| IndexBuildStatus {
            handle: IndexHandle::new(id.clone(), candidate.tree.generation),
            state: if candidate.failure.is_some() {
                IndexState::Failed
            } else {
                IndexState::Building
            },
            progress: candidate.progress,
            failure: candidate.failure,
        });
        Ok(IndexStatus {
            definition: self.definition,
            active,
            candidate,
        })
    }
}

impl Snapshot {
    pub(crate) fn check_index_limits(&self) -> Result<()> {
        if self.manifest.index_count > self.indexes.live_indexes() {
            return Err(Error::ResourceLimit("configured live index count"));
        }
        Ok(())
    }
    pub(crate) async fn load_index(
        &self,
        id: &IndexId,
        budget: &mut Budget,
    ) -> Result<Option<LoadedIndex>> {
        self.check_index_limits()?;
        let Some(value) = metadata::get(
            self.store.as_ref(),
            Family::IndexDefinitions,
            self.manifest.catalog.as_ref(),
            &metadata::index_definition_key(id),
            budget,
        )
        .await?
        else {
            return Ok(None);
        };
        self.load_catalog_value(id, &value, budget).await.map(Some)
    }
    async fn load_catalog_value(
        &self,
        id: &IndexId,
        value: &[u8],
        budget: &mut Budget,
    ) -> Result<LoadedIndex> {
        if value.len() > metadata::MAX_VALUE_BYTES {
            return Err(corrupt("catalog reference size"));
        }
        let initial = budget.decoded;
        let mut reader = format::Reader::bounded(value, initial);
        let parsed = ObjectRef::decode(&mut reader, lifecycle::MAX_DESCRIPTOR_BYTES);
        budget.decoded -= initial - reader.remaining();
        let reference = parsed?;
        reader.finish()?;
        budget.charge_decoded(reference.len as usize)?;
        let bytes = reference
            .read(self.store.as_ref(), lifecycle::MAX_DESCRIPTOR_BYTES, budget)
            .await?;
        let descriptor = lifecycle::decode_index_descriptor(&bytes, &mut budget.decoded)?;
        if descriptor.id != *id
            || descriptor.database_id != *self.database_id()
            || descriptor
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.base.sequence >= self.sequence())
        {
            return Err(corrupt("catalog descriptor identity/base mismatch"));
        }
        budget.charge_decoded(descriptor.definition.len as usize)?;
        let bytes = descriptor
            .definition
            .read(self.store.as_ref(), metadata::MAX_VALUE_BYTES, budget)
            .await?;
        let definition = metadata::decode_index_definition_budget(&bytes, &mut budget.decoded)?;
        lifecycle::validate_descriptor_definition(&descriptor, &definition)?;
        Ok(LoadedIndex {
            descriptor,
            definition,
        })
    }
    pub(crate) async fn catalog(&self, budget: &mut Budget) -> Result<Vec<LoadedIndex>> {
        self.check_index_limits()?;
        let rows = metadata::range(
            self.store.as_ref(),
            Family::IndexDefinitions,
            self.manifest.catalog.as_ref(),
            KeyRange {
                start: Bound::Unbounded,
                end: None,
            },
            MAX_LIVE_INDEXES + 1,
            budget,
        )
        .await?;
        if rows.len() != self.manifest.index_count {
            return Err(corrupt("catalog count mismatch"));
        }
        budget.charge_decoded(rows.len() * std::mem::size_of::<LoadedIndex>())?;
        let mut indexes = Vec::with_capacity(rows.len());
        let mut builds = 0;
        for (key, value) in rows {
            budget.charge_decoded(key.len() + 128)?;
            let id = metadata::index_definition_id(&key)?;
            let index = self.load_catalog_value(&id, &value, budget).await?;
            builds += usize::from(
                index
                    .descriptor
                    .candidate
                    .as_ref()
                    .is_some_and(|candidate| candidate.failure.is_none()),
            );
            if builds > MAX_INDEX_BUILDS {
                return Err(corrupt("too many building index generations"));
            }
            indexes.push(index);
        }
        Ok(indexes)
    }
    pub async fn get_index(&self, id: &IndexId) -> Result<Option<IndexStatus>> {
        let mut budget = self.resources.budget();
        self.load_index(id, &mut budget)
            .await?
            .map(|index| index.status(&mut budget))
            .transpose()
    }
    /// At most MAX_LIVE_INDEXES entries, all from this pinned snapshot.
    pub async fn list_indexes(&self) -> Result<Vec<IndexStatus>> {
        let mut budget = self.resources.budget();
        self.catalog(&mut budget)
            .await?
            .into_iter()
            .map(|index| index.status(&mut budget))
            .collect()
    }
}

pub(crate) fn stage_descriptor(
    descriptor: &IndexDescriptor,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<Vec<u8>> {
    // Reserve encoder buffers before allocating; the decoder charges its own output.
    budget.charge_decoded(2 * lifecycle::MAX_DESCRIPTOR_BYTES)?;
    let bytes = lifecycle::encode_index_descriptor(descriptor)?;
    lifecycle::decode_index_descriptor(&bytes, &mut budget.decoded)?;
    metadata::charge(
        &mut budget.prepared,
        bytes.len(),
        "prepared index descriptor bytes",
    )?;
    let id = ObjectId::random();
    let reference = ObjectRef::new(id, &bytes);
    objects.push((id, bytes.into()));
    let mut writer = format::Writer::default();
    reference.write(&mut writer);
    Ok(writer.0)
}

pub(crate) async fn prepare_catalog(
    snapshot: &Snapshot,
    root: &mut RootV3,
    changes: &metadata::Changes,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<()> {
    let prepared = metadata::prepare(
        snapshot.store.as_ref(),
        Family::IndexDefinitions,
        root.catalog.as_ref(),
        changes,
        budget,
    )
    .await?;
    root.catalog = prepared.root;
    objects.extend(prepared.objects);
    Ok(())
}
