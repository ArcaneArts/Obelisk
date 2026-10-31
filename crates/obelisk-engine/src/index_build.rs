//! One resumable source-snapshot enumeration step, reconciled against current data.
use crate::{
    Engine, Error, Result, Snapshot, corrupt,
    index_catalog::{LoadedIndex, Objects},
    index_maintenance::{existing_entry, oversized_key, prepare_entry},
    metadata::{self, Budget, Family, KeyRange},
};
use obelisk_storage::VersionToken;
use obelisk_types::*;
use std::ops::Bound;

pub(crate) async fn advance(
    engine: &Engine,
    snapshot: &Snapshot,
    index: &mut LoadedIndex,
    options: IndexBuildOptions,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<IndexBuildStatus> {
    let definition = &index.definition;
    let candidate = index
        .descriptor
        .candidate
        .as_mut()
        .ok_or_else(|| corrupt("missing build candidate"))?;
    budget.charge_decoded(2 * metadata::MAX_KEY_BYTES + 2048)?;
    let base = engine
        .load_snapshot(candidate.base.clone(), VersionToken(String::new()), budget)
        .await?;
    let (family, root, prefix) = match definition.scope() {
        IndexScope::Children(scope) => (
            Family::Membership,
            base.manifest.membership.as_ref(),
            metadata::parent_prefix(match scope {
                QueryScope::Root => None,
                QueryScope::Path(path) => Some(path),
            }),
        ),
        IndexScope::Descendants(scope) => (
            Family::Documents,
            base.manifest.documents.as_ref(),
            match scope {
                QueryScope::Root => Vec::new(),
                QueryScope::Path(path) => metadata::document_key(path),
            },
        ),
    };
    let after = candidate.progress.last_path().map(|path| match family {
        Family::Membership => metadata::membership_key(path),
        _ => metadata::document_key(path),
    });
    if let Some(after) = after.as_ref()
        && metadata::get(snapshot.store.as_ref(), family, root, after, budget)
            .await?
            .is_none()
    {
        return Err(corrupt("index build position absent from source snapshot"));
    }
    let end = metadata::prefix_end(&prefix);
    let start = if let Some(after) = after.as_deref() {
        Bound::Excluded(after)
    } else if family == Family::Documents && !prefix.is_empty() {
        Bound::Excluded(prefix.as_slice())
    } else {
        Bound::Included(prefix.as_slice())
    };
    let rows = metadata::range(
        snapshot.store.as_ref(),
        family,
        root,
        KeyRange {
            start,
            end: end.as_deref(),
        },
        options.documents() + 1,
        budget,
    )
    .await?;
    let total = rows.len();
    let mut changes = metadata::Changes::new();
    let mut source_bytes = 0;
    let mut examined = 0;
    let mut last = None;
    let initial_objects = objects.len();
    for (key, value) in rows.into_iter().take(options.documents()) {
        budget
            .charge_decoded(key.len() + key.iter().filter(|byte| **byte == 0).count() * 64 + 256)?;
        let path = match family {
            Family::Membership => {
                if !value.is_empty() {
                    return Err(corrupt("invalid source membership marker"));
                }
                metadata::membership_path(&key)?
            }
            _ => {
                metadata::decode_location(&value)?;
                metadata::document_path(&key)?
            }
        };
        if !definition.scope().contains(&path) {
            return Err(corrupt("index source outside definition scope"));
        }
        let location = snapshot.location(&path, budget).await?;
        let cost = location.as_ref().map_or(0, |location| {
            (location.range.end - location.range.start) as usize
        });
        if cost > options.source_bytes() - source_bytes {
            if examined == 0 {
                return Err(Error::ResourceLimit(
                    "document exceeds index build source bytes",
                ));
            }
            break;
        }
        source_bytes += cost;
        if let Some(location) = location {
            let document = snapshot
                .read_location(&path, location.clone(), budget)
                .await?
                .ok_or_else(|| corrupt("index source document disappeared"))?;
            budget.charge_decoded(metadata::MAX_INDEX_SCALAR_BYTES + key.len())?;
            let key =
                match metadata::index_entry_for_document(definition, &path, &document.document) {
                    Err(error) if oversized_key(&error) => {
                        // A semantic failure publishes no partial chunk. The original
                        // progress/tree and any old active generation remain intact.
                        candidate.failure = Some(IndexBuildFailure::ScalarKeyTooLarge(path));
                        objects.truncate(initial_objects);
                        budget.charge_decoded(2 * MAX_PATH_BYTES + 2048)?;
                        return Ok(IndexBuildStatus {
                            handle: IndexHandle::new(
                                definition.id().clone(),
                                candidate.tree.generation.clone(),
                            ),
                            state: IndexState::Failed,
                            progress: candidate.progress.clone(),
                            failure: candidate.failure.clone(),
                        });
                    }
                    other => other?,
                };
            if let Some(key) = key
                && existing_entry(
                    snapshot,
                    &candidate.tree,
                    &key,
                    &location,
                    definition,
                    budget,
                )
                .await?
                .is_none()
            {
                let value = prepare_entry(
                    &candidate.tree,
                    definition,
                    &path,
                    &document.document,
                    &location,
                    objects,
                    budget,
                )?;
                changes.insert(key, Some(value));
            }
        }
        examined += 1;
        last = Some(path);
    }
    let prepared = metadata::prepare(
        snapshot.store.as_ref(),
        Family::ScalarIndexes,
        candidate.tree.entries.as_ref(),
        &changes,
        budget,
    )
    .await?;
    candidate.tree.entries = prepared.root;
    objects.extend(prepared.objects);
    if examined > 0 {
        candidate.progress = IndexBuildProgress::new(
            candidate
                .progress
                .examined()
                .checked_add(examined as u64)
                .ok_or_else(|| corrupt("index build count overflow"))?,
            last,
        )?;
    }
    budget.charge_decoded(2 * MAX_PATH_BYTES + 2048)?;
    let status = IndexBuildStatus {
        handle: IndexHandle::new(definition.id().clone(), candidate.tree.generation.clone()),
        state: if examined == total {
            IndexState::Active
        } else {
            IndexState::Building
        },
        progress: candidate.progress.clone(),
        failure: None,
    };
    if status.state == IndexState::Active {
        let candidate = index
            .descriptor
            .candidate
            .take()
            .ok_or_else(|| corrupt("missing activation candidate"))?;
        index.descriptor.active = Some(candidate.tree);
    }
    Ok(status)
}
