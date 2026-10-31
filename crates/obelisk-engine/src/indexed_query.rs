//! Snapshot-pinned scalar queries, bounded pages and typed covered projections.
use crate::{
    Engine, Error, Result, Snapshot, SnapshotReference, corrupt, format,
    index_catalog::LoadedIndex,
    indexed_plan,
    metadata::{
        self, Budget, Family, KeyRange,
        lifecycle::{self, IndexEntry},
    },
};
use obelisk_storage::VersionToken;
use obelisk_types::*;
use std::ops::Bound;

/// Native opaque continuation; no portable decoder or server-side session.
#[derive(Debug, Clone)]
pub struct IndexedQueryCursor {
    snapshot: SnapshotReference,
    identity: [u8; 32],
    index: IndexHandle,
    version: u16,
    last_key: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct IndexedQueryPage {
    pub snapshot: SnapshotReference,
    pub index: IndexHandle,
    pub entries: Vec<QueryEntry>,
    pub next: Option<IndexedQueryCursor>,
}
fn validate_identity(cursor: Option<&IndexedQueryCursor>, identity: &[u8; 32]) -> Result<()> {
    if cursor.is_some_and(|c| c.version != 1 || &c.identity != identity) {
        return Err(Error::InvalidCursor("indexed query mismatch"));
    }
    Ok(())
}
impl Engine {
    pub async fn query(
        &self,
        query: &IndexedQuery,
        options: PageOptions,
        cursor: Option<&IndexedQueryCursor>,
    ) -> Result<IndexedQueryPage> {
        let mut budget = self.options.resources.budget();
        let identity = indexed_plan::identity(query, &mut budget)?;
        validate_identity(cursor, &identity)?;
        let current = self.snapshot_with_budget(&mut budget).await?;
        let snapshot = if let Some(cursor) = cursor {
            if cursor.snapshot.0.database_id != *current.database_id() {
                return Err(Error::InvalidCursor("database mismatch"));
            }
            if cursor.snapshot == current.reference() {
                current
            } else {
                self.load_snapshot(
                    cursor.snapshot.0.clone(),
                    VersionToken(String::new()),
                    &mut budget,
                )
                .await?
            }
        } else {
            current
        };
        snapshot
            .indexed_page(query, options, cursor, identity, &mut budget)
            .await
    }
}
impl Snapshot {
    pub async fn query(
        &self,
        query: &IndexedQuery,
        options: PageOptions,
        cursor: Option<&IndexedQueryCursor>,
    ) -> Result<IndexedQueryPage> {
        let mut budget = self.resources.budget();
        let identity = indexed_plan::identity(query, &mut budget)?;
        validate_identity(cursor, &identity)?;
        self.indexed_page(query, options, cursor, identity, &mut budget)
            .await
    }
    async fn indexed_page(
        &self,
        query: &IndexedQuery,
        options: PageOptions,
        cursor: Option<&IndexedQueryCursor>,
        identity: [u8; 32],
        budget: &mut Budget,
    ) -> Result<IndexedQueryPage> {
        if cursor.is_some_and(|c| c.snapshot != self.reference()) {
            return Err(Error::InvalidCursor("snapshot mismatch"));
        }
        let index = indexed_plan::choose(self, query, budget).await?;
        let tree = index
            .descriptor
            .active
            .as_ref()
            .ok_or_else(|| corrupt("planner selected inactive index"))?;
        let handle = IndexHandle::new(index.definition.id().clone(), tree.generation.clone());
        if cursor.is_some_and(|c| c.index != handle) {
            return Err(Error::InvalidCursor("index generation mismatch"));
        }
        let bounds = indexed_plan::bounds(&index.definition, query)?;
        if let Some(cursor) = cursor {
            budget.charge_decoded(8 * cursor.last_key.len() + 2048)?;
            let (_, path) = metadata::decode_index_entry_key(&index.definition, &cursor.last_key)
                .map_err(|_| Error::InvalidCursor("invalid indexed position"))?;
            if !index.definition.scope().contains(&path) || !bounds.contains(&cursor.last_key) {
                return Err(Error::InvalidCursor("indexed position outside query"));
            }
            if metadata::get(
                self.store.as_ref(),
                Family::ScalarIndexes,
                tree.entries.as_ref(),
                &cursor.last_key,
                budget,
            )
            .await?
            .is_none()
            {
                return Err(Error::InvalidCursor("indexed position not in snapshot"));
            }
        }
        let rows = if bounds.empty {
            Vec::new()
        } else {
            metadata::range(
                self.store.as_ref(),
                Family::ScalarIndexes,
                tree.entries.as_ref(),
                KeyRange {
                    start: cursor.map_or(Bound::Included(bounds.start.as_slice()), |c| {
                        Bound::Excluded(c.last_key.as_slice())
                    }),
                    end: bounds.end.as_deref(),
                },
                options.limit() + 1,
                budget,
            )
            .await?
        };
        let total = rows.len();
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut last_key = None;
        for (key, value) in rows.into_iter().take(options.limit()) {
            budget.charge_decoded(8 * key.len() + 2048)?;
            let (values, path) = metadata::decode_index_entry_key(&index.definition, &key)?;
            if !bounds.contains(&key) || !index.definition.scope().contains(&path) {
                return Err(corrupt("index key outside query scope"));
            }
            let entry = lifecycle::decode_index_entry(&value, &mut budget.decoded)?;
            if entry.generation != tree.generation
                || entry.projection.is_some() == index.definition.covered_fields().is_empty()
            {
                return Err(corrupt("index generation/coverage mismatch"));
            }
            if self.location(&path, budget).await?.as_ref() != Some(&entry.location) {
                return Err(corrupt("index location/revision mismatch"));
            }
            // A full record bounds every subset projection. If an uncovered
            // projection's next payload might exceed the remaining space, stop
            // conservatively before reading it. An empty page still reads one
            // such payload so a small projection of a large document can fit.
            let full = matches!(query.projection(), Projection::Full);
            if !projection_is_covered(&index.definition, query.projection())
                && bytes + full_bytes(&entry) > options.max_bytes()
            {
                if !entries.is_empty() {
                    break;
                }
                if full {
                    return Err(Error::ResourceLimit("document exceeds indexed page bytes"));
                }
            }
            let document = self
                .indexed_document(
                    &index,
                    query.projection(),
                    &key,
                    &path,
                    &values,
                    &entry,
                    budget,
                )
                .await?;
            let size = format::HEADER_BYTES
                + 5
                + path.segments().iter().map(|s| s.len() + 4).sum::<usize>()
                + document.estimated_size()?
                + 4
                + entry.location.revision.as_str().len();
            if bytes + size > options.max_bytes() {
                if entries.is_empty() {
                    return Err(Error::ResourceLimit("document exceeds indexed page bytes"));
                }
                break;
            }
            budget.charge_decoded(std::mem::size_of::<QueryEntry>() + key.len())?;
            entries.push(QueryEntry {
                path,
                document,
                revision: entry.location.revision,
            });
            bytes += size;
            last_key = Some(key);
        }
        budget.charge_decoded(4096)?; // Owned result/cursor snapshot and handle identities.
        let next = if total > entries.len() {
            Some(IndexedQueryCursor {
                snapshot: self.reference(),
                identity,
                index: handle.clone(),
                version: 1,
                last_key: last_key
                    .ok_or_else(|| corrupt("indexed page failed to make progress"))?,
            })
        } else {
            None
        };
        Ok(IndexedQueryPage {
            snapshot: self.reference(),
            index: handle,
            entries,
            next,
        })
    }
    #[allow(clippy::too_many_arguments)]
    async fn indexed_document(
        &self,
        index: &LoadedIndex,
        projection: &Projection,
        key: &[u8],
        path: &DocumentPath,
        values: &[Value],
        entry: &IndexEntry,
        budget: &mut Budget,
    ) -> Result<Document> {
        let definition = &index.definition;
        let fields = match projection {
            Projection::Full => None,
            Projection::Fields(fields) => Some(fields),
        };
        let covered = projection_is_covered(definition, projection);
        if covered {
            let fields = fields.unwrap();
            let needs_projection = fields
                .iter()
                .any(|field| definition.covered_fields().contains(field));
            let stored = if needs_projection {
                let reference = entry
                    .projection
                    .as_ref()
                    .ok_or_else(|| corrupt("covered projection missing"))?;
                budget.charge_decoded(reference.len as usize)?;
                let bytes = reference
                    .read(self.store.as_ref(), lifecycle::MAX_PROJECTION_BYTES, budget)
                    .await?;
                let stored = lifecycle::decode_projection(&bytes, &mut budget.decoded)?;
                if stored.generation != entry.generation
                    || stored.path != *path
                    || stored.revision != entry.location.revision
                    || stored
                        .document
                        .fields()
                        .keys()
                        .any(|field| !definition.covered_fields().contains(field))
                {
                    return Err(corrupt("covered projection identity/fields mismatch"));
                }
                Some(stored.document)
            } else {
                None
            };
            let selected = || {
                fields.iter().filter_map(|field| {
                    definition
                        .fields()
                        .iter()
                        .position(|f| f.path() == field)
                        .map(|p| &values[p])
                        .or_else(|| stored.as_ref().and_then(|d| d.get(field)))
                        .map(|v| (field, v))
                })
            };
            budget.charge_decoded(format::selected_fields_allocation(selected()))?;
            return Document::new(selected().map(|(f, v)| (f.clone(), v.clone())))
                .map_err(|_| corrupt("conflicting projected fields"));
        }
        let current = self
            .read_location(path, entry.location.clone(), budget)
            .await?
            .ok_or_else(|| corrupt("missing indexed document"))?;
        budget.charge_decoded(metadata::MAX_KEY_BYTES)?;
        if metadata::index_entry_for_document(definition, path, &current.document)?.as_deref()
            != Some(key)
        {
            return Err(corrupt("indexed values disagree with document"));
        }
        match fields {
            None => Ok(current.document),
            Some(fields) => {
                let selected = || {
                    fields
                        .iter()
                        .filter_map(|field| current.document.get(field).map(|v| (field, v)))
                };
                budget.charge_decoded(format::selected_fields_allocation(selected()))?;
                Ok(Document::new(
                    selected().map(|(f, v)| (f.clone(), v.clone())),
                )?)
            }
        }
    }
}
fn projection_is_covered(definition: &IndexDefinition, projection: &Projection) -> bool {
    matches!(projection, Projection::Fields(fields) if fields.iter().all(|field|
        definition.fields().iter().any(|f| f.path() == field)
            || definition.covered_fields().contains(field)))
}
fn full_bytes(entry: &IndexEntry) -> usize {
    (entry.location.range.end - entry.location.range.start) as usize
        + 4
        + entry.location.revision.as_str().len()
}

#[cfg(all(test, unix))]
#[path = "indexed_query_tests.rs"]
mod tests;
