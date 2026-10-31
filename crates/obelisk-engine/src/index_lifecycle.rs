//! Idempotent management operations share the document publisher and request namespace.
use crate::{
    Engine, Error, Result, Snapshot, corrupt,
    index_catalog::{LoadedIndex, Objects, prepare_catalog, stage_descriptor},
    metadata::{
        self, Budget, Family, ObjectRef, OutcomeRef,
        lifecycle::{
            self, IndexCandidate, IndexDescriptor, IndexGenerationRoot, RootV3, TaggedOutcome,
        },
    },
};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::collections::BTreeMap;

impl Engine {
    pub async fn get_index(&self, id: &IndexId) -> Result<Option<IndexStatus>> {
        let mut budget = self.options.resources.budget();
        self.snapshot_with_budget(&mut budget)
            .await?
            .load_index(id, &mut budget)
            .await?
            .map(|index| index.status(&mut budget))
            .transpose()
    }
    pub async fn list_indexes(&self) -> Result<Vec<IndexStatus>> {
        let mut budget = self.options.resources.budget();
        self.snapshot_with_budget(&mut budget)
            .await?
            .catalog(&mut budget)
            .await?
            .into_iter()
            .map(|index| index.status(&mut budget))
            .collect()
    }
    pub async fn create_index(
        &self,
        request: &RequestId,
        definition: IndexDefinition,
    ) -> Result<IndexReceipt> {
        self.index_operation(request, &IndexCommand::Create(definition))
            .await
    }
    pub async fn advance_index_build(
        &self,
        request: &RequestId,
        handle: &IndexHandle,
        options: IndexBuildOptions,
    ) -> Result<IndexReceipt> {
        self.index_operation(request, &IndexCommand::Advance(handle.clone(), options))
            .await
    }
    pub async fn rebuild_index(
        &self,
        request: &RequestId,
        handle: &IndexHandle,
    ) -> Result<IndexReceipt> {
        self.index_operation(request, &IndexCommand::Rebuild(handle.clone()))
            .await
    }
    pub async fn drop_index(
        &self,
        request: &RequestId,
        handle: &IndexHandle,
    ) -> Result<IndexReceipt> {
        self.index_operation(request, &IndexCommand::Drop(handle.clone()))
            .await
    }
    /// None does not exclude an operation still in flight. Retry the same command.
    pub async fn resolve_index_operation(
        &self,
        request: &RequestId,
    ) -> Result<Option<IndexReceipt>> {
        let mut budget = self.options.resources.budget();
        let snapshot = self.snapshot_with_budget(&mut budget).await?;
        let Some(reference) = snapshot.outcome_ref(request, &mut budget).await? else {
            return Ok(None);
        };
        match snapshot
            .request_outcome(request, &reference, &mut budget)
            .await?
        {
            TaggedOutcome::Index { receipt, .. } => Ok(Some(receipt)),
            TaggedOutcome::Document(_) => Err(Error::RequestKindMismatch(request.clone())),
        }
    }

    async fn index_operation(
        &self,
        request: &RequestId,
        command: &IndexCommand,
    ) -> Result<IndexReceipt> {
        let fingerprint = lifecycle::index_command_fingerprint(command)?;
        let mut attempted = false;
        for attempt in 0..self.options.max_attempts {
            let mut budget = self.options.resources.budget();
            let snapshot = self
                .snapshot_with_budget(&mut budget)
                .await
                .map_err(|e| after_attempt(request, e, attempted))?;
            if let Some(receipt) = existing(&snapshot, request, &fingerprint, &mut budget)
                .await
                .map_err(|e| after_attempt(request, e, attempted))?
            {
                return Ok(receipt);
            }
            budget.charge_decoded(2 * metadata::MAX_ROOT_BYTES)?;
            let mut root = snapshot.manifest.clone();
            root.sequence = root
                .sequence
                .checked_add(1)
                .ok_or_else(|| corrupt("commit sequence overflow"))?;
            root.commit_id = CommitId::new(uuid::Uuid::new_v4().to_string())?;
            let mut objects = Vec::new();
            let receipt = prepare_operation(
                self,
                &snapshot,
                &mut root,
                request,
                command,
                &mut objects,
                &mut budget,
            )
            .await?;
            prepare_outcome(
                &snapshot,
                &mut root,
                TaggedOutcome::Index {
                    fingerprint,
                    receipt: receipt.clone(),
                },
                &mut objects,
                &mut budget,
            )
            .await?;
            match self
                .publish_root(&snapshot, &root, objects, &mut budget, &mut attempted)
                .await
            {
                Ok(()) => return Ok(receipt),
                Err(Error::Storage(obelisk_storage::Error::Conflict)) => {
                    self.options.backoff(attempt).await
                }
                Err(error) => {
                    if !attempted {
                        return Err(error);
                    }
                    let mut resolution = self.options.resources.budget();
                    if let Ok(snapshot) = self.snapshot_with_budget(&mut resolution).await {
                        match existing(&snapshot, request, &fingerprint, &mut resolution).await {
                            Ok(Some(receipt)) => return Ok(receipt),
                            Err(error @ Error::RequestIdReuse(_)) => return Err(error),
                            _ => {}
                        }
                    }
                    return Err(uncertain(request, error));
                }
            }
        }
        let mut budget = self.options.resources.budget();
        let snapshot = self
            .snapshot_with_budget(&mut budget)
            .await
            .map_err(|e| uncertain(request, e))?;
        existing(&snapshot, request, &fingerprint, &mut budget)
            .await
            .map_err(|e| after_attempt(request, e, true))?
            .ok_or(Error::RetryExhausted)
    }
}

fn uncertain(request: &RequestId, error: Error) -> Error {
    Error::Uncertain {
        request_id: request.clone(),
        reason: error.to_string(),
    }
}
fn after_attempt(request: &RequestId, error: Error, attempted: bool) -> Error {
    if attempted && !matches!(error, Error::RequestIdReuse(_)) {
        uncertain(request, error)
    } else {
        error
    }
}
async fn existing(
    snapshot: &Snapshot,
    request: &RequestId,
    fingerprint: &[u8; 32],
    budget: &mut Budget,
) -> Result<Option<IndexReceipt>> {
    let Some(reference) = snapshot.outcome_ref(request, budget).await? else {
        return Ok(None);
    };
    if reference.fingerprint != *fingerprint {
        return Err(Error::RequestIdReuse(request.clone()));
    }
    match snapshot
        .request_outcome(request, &reference, budget)
        .await?
    {
        TaggedOutcome::Index { receipt, .. } => Ok(Some(receipt)),
        TaggedOutcome::Document(_) => Err(Error::RequestKindMismatch(request.clone())),
    }
}

fn new_candidate(snapshot: &Snapshot, generation: IndexGeneration) -> IndexCandidate {
    IndexCandidate {
        tree: IndexGenerationRoot {
            generation,
            entries: None,
        },
        base: snapshot.reference.clone(),
        progress: IndexBuildProgress::default(),
        failure: None,
    }
}
fn matches_handle(index: &LoadedIndex, handle: &IndexHandle) -> bool {
    index.definition.id() == handle.id()
        && (index
            .descriptor
            .active
            .as_ref()
            .is_some_and(|tree| &tree.generation == handle.generation())
            || index
                .descriptor
                .candidate
                .as_ref()
                .is_some_and(|candidate| &candidate.tree.generation == handle.generation()))
}

async fn prepare_operation(
    engine: &Engine,
    snapshot: &Snapshot,
    root: &mut RootV3,
    request: &RequestId,
    command: &IndexCommand,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<IndexReceipt> {
    let mut indexes = snapshot.catalog(budget).await?;
    let id = match command {
        IndexCommand::Create(definition) => definition.id(),
        IndexCommand::Advance(handle, _)
        | IndexCommand::Rebuild(handle)
        | IndexCommand::Drop(handle) => handle.id(),
    };
    let position = indexes.iter().position(|index| index.definition.id() == id);
    let building = indexes.iter().any(|index| {
        index
            .descriptor
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.failure.is_none())
    });
    let mut changes = metadata::Changes::new();
    let status = match command {
        IndexCommand::Create(definition) => {
            if position.is_some() {
                return Err(Error::IndexAlreadyExists(id.clone()));
            }
            if root.index_count >= engine.options.indexes.live_indexes() {
                return Err(Error::ResourceLimit("live index count"));
            }
            if building {
                return Err(Error::IndexBuildInProgress);
            }
            budget.charge_decoded(2 * metadata::MAX_VALUE_BYTES)?;
            let bytes = metadata::encode_index_definition(definition)?;
            metadata::decode_index_definition_budget(&bytes, &mut budget.decoded)?;
            metadata::charge(
                &mut budget.prepared,
                bytes.len(),
                "prepared index definition bytes",
            )?;
            let object = ObjectId::random();
            let reference = ObjectRef::new(object, &bytes);
            objects.push((object, bytes.into()));
            let generation = IndexGeneration::new(root.commit_id.as_str())?;
            let descriptor = IndexDescriptor {
                database_id: snapshot.database_id().clone(),
                id: id.clone(),
                definition: reference,
                active: None,
                candidate: Some(new_candidate(snapshot, generation.clone())),
            };
            let value = stage_descriptor(&descriptor, objects, budget)?;
            changes.insert(metadata::index_definition_key(id), Some(value));
            root.index_count += 1;
            IndexBuildStatus {
                handle: IndexHandle::new(id.clone(), generation),
                state: IndexState::Building,
                progress: IndexBuildProgress::default(),
                failure: None,
            }
        }
        IndexCommand::Advance(handle, options) => {
            let position = position.ok_or_else(|| Error::IndexNotFound(id.clone()))?;
            let index = &mut indexes[position];
            if !index
                .descriptor
                .candidate
                .as_ref()
                .is_some_and(|candidate| {
                    &candidate.tree.generation == handle.generation() && candidate.failure.is_none()
                })
            {
                return Err(Error::IndexStateConflict(handle.clone()));
            }
            let status =
                crate::index_build::advance(engine, snapshot, index, *options, objects, budget)
                    .await?;
            changes.insert(
                metadata::index_definition_key(id),
                Some(stage_descriptor(&index.descriptor, objects, budget)?),
            );
            status
        }
        IndexCommand::Rebuild(handle) => {
            let position = position.ok_or_else(|| Error::IndexNotFound(id.clone()))?;
            let index = &mut indexes[position];
            if !matches_handle(index, handle) {
                return Err(Error::IndexStateConflict(handle.clone()));
            }
            if building {
                return Err(Error::IndexBuildInProgress);
            }
            let generation = IndexGeneration::new(root.commit_id.as_str())?;
            index.descriptor.candidate = Some(new_candidate(snapshot, generation.clone()));
            changes.insert(
                metadata::index_definition_key(id),
                Some(stage_descriptor(&index.descriptor, objects, budget)?),
            );
            IndexBuildStatus {
                handle: IndexHandle::new(id.clone(), generation),
                state: IndexState::Building,
                progress: IndexBuildProgress::default(),
                failure: None,
            }
        }
        IndexCommand::Drop(handle) => {
            let position = position.ok_or_else(|| Error::IndexNotFound(id.clone()))?;
            let index = &mut indexes[position];
            if !matches_handle(index, handle) {
                return Err(Error::IndexStateConflict(handle.clone()));
            }
            budget.charge_decoded(MAX_PATH_BYTES + 2048)?;
            let progress = index
                .descriptor
                .candidate
                .as_ref()
                .map(|candidate| candidate.progress.clone())
                .unwrap_or_default();
            changes.insert(metadata::index_definition_key(id), None);
            root.index_count = root
                .index_count
                .checked_sub(1)
                .ok_or_else(|| corrupt("catalog count underflow"))?;
            IndexBuildStatus {
                handle: handle.clone(),
                state: IndexState::Dropped,
                progress,
                failure: None,
            }
        }
    };
    prepare_catalog(snapshot, root, &changes, objects, budget).await?;
    budget.charge_decoded(2 * MAX_PATH_BYTES + 2048)?;
    Ok(IndexReceipt {
        publication: CommitReceipt {
            database_id: snapshot.database_id().clone(),
            request_id: request.clone(),
            commit_id: root.commit_id.clone(),
            sequence: root.sequence,
            revisions: BTreeMap::new(),
        },
        handle: status.handle,
        operation: command.operation(),
        state: status.state,
        progress: status.progress,
        failure: status.failure,
    })
}

async fn prepare_outcome(
    snapshot: &Snapshot,
    root: &mut RootV3,
    outcome: TaggedOutcome,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<()> {
    budget.charge_decoded(3 * lifecycle::MAX_DESCRIPTOR_BYTES)?;
    let bytes = lifecycle::encode_tagged_outcome(&outcome)?;
    lifecycle::decode_tagged_outcome(&bytes, &mut budget.decoded)?;
    metadata::charge(
        &mut budget.prepared,
        bytes.len(),
        "prepared index outcome bytes",
    )?;
    let id = ObjectId::random();
    let reference = OutcomeRef {
        fingerprint: *outcome.fingerprint(),
        blob: ObjectRef::new(id, &bytes),
    };
    let changes = BTreeMap::from([(
        outcome
            .publication()
            .request_id
            .as_str()
            .as_bytes()
            .to_vec(),
        Some(reference.encode()?),
    )]);
    objects.push((id, bytes.into()));
    let prepared = metadata::prepare(
        snapshot.store.as_ref(),
        Family::Requests,
        root.requests.as_ref(),
        &changes,
        budget,
    )
    .await?;
    root.requests = prepared.root;
    objects.extend(prepared.objects);
    Ok(())
}
