//! Prepare document-dependent index changes without uploading or publishing.
use crate::{
    Error, Result, Snapshot, corrupt, format,
    index_catalog::{LoadedIndex, Objects, prepare_catalog, stage_descriptor},
    metadata::{
        self, Budget, Family,
        lifecycle::{self, CoveredProjection, IndexEntry, IndexGenerationRoot, RootV3},
    },
};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use std::collections::BTreeMap;

pub(crate) fn oversized_key(error: &Error) -> bool {
    matches!(error, Error::ResourceLimit("scalar index key bytes"))
}

pub(crate) async fn existing_entry(
    snapshot: &Snapshot,
    tree: &IndexGenerationRoot,
    key: &[u8],
    location: &format::Location,
    definition: &IndexDefinition,
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>> {
    let value = metadata::get(
        snapshot.store.as_ref(),
        Family::ScalarIndexes,
        tree.entries.as_ref(),
        key,
        budget,
    )
    .await?;
    if let Some(value) = &value {
        let entry = lifecycle::decode_index_entry(value, &mut budget.decoded)?;
        if entry.generation != tree.generation
            || entry.location != *location
            || entry.projection.is_some() == definition.covered_fields().is_empty()
        {
            return Err(corrupt("index entry generation/location/coverage mismatch"));
        }
    }
    Ok(value)
}

pub(crate) fn prepare_entry(
    tree: &IndexGenerationRoot,
    definition: &IndexDefinition,
    path: &DocumentPath,
    document: &Document,
    location: &format::Location,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<Vec<u8>> {
    let projection = if definition.covered_fields().is_empty() {
        None
    } else {
        let selected = || {
            definition
                .covered_fields()
                .iter()
                .filter_map(|field| document.get(field).map(|value| (field, value)))
        };
        budget.charge_decoded(
            format::selected_fields_allocation(selected())
                + 1024
                + metadata::document_key_len(path),
        )?;
        let projected =
            Document::new(selected().map(|(field, value)| (field.clone(), value.clone())))?;
        budget.charge_decoded(2 * (projected.estimated_size()? + 2 * MAX_PATH_BYTES + 2048))?;
        let projection = CoveredProjection {
            generation: tree.generation.clone(),
            path: path.clone(),
            revision: location.revision.clone(),
            document: projected,
        };
        let bytes = lifecycle::encode_projection(&projection)?;
        lifecycle::decode_projection(&bytes, &mut budget.decoded)?;
        metadata::charge(
            &mut budget.prepared,
            bytes.len(),
            "prepared covered projection bytes",
        )?;
        let id = ObjectId::random();
        let reference = metadata::ObjectRef::new(id, &bytes);
        objects.push((id, bytes.into()));
        Some(reference)
    };
    budget.charge_decoded(2048)?;
    let entry = IndexEntry {
        generation: tree.generation.clone(),
        location: location.clone(),
        projection,
    };
    let bytes = lifecycle::encode_index_entry(&entry)?;
    lifecycle::decode_index_entry(&bytes, &mut budget.decoded)?;
    Ok(bytes)
}

pub(crate) struct DocumentChanges<'a> {
    pub before: &'a BTreeMap<DocumentPath, Option<VersionedDocument>>,
    pub previous_locations: &'a BTreeMap<DocumentPath, Option<format::Location>>,
    pub after: &'a BTreeMap<DocumentPath, Option<Document>>,
    pub locations: &'a BTreeMap<DocumentPath, format::Location>,
}

pub(crate) async fn maintain(
    snapshot: &Snapshot,
    root: &mut RootV3,
    indexes: Vec<LoadedIndex>,
    documents: DocumentChanges<'_>,
    objects: &mut Objects,
    budget: &mut Budget,
) -> Result<()> {
    let mut catalog = metadata::Changes::new();
    for mut index in indexes {
        let mut changed = false;
        let definition = &index.definition;
        let active = index.descriptor.active.iter_mut().map(|tree| (tree, true));
        let candidate = index
            .descriptor
            .candidate
            .iter_mut()
            .filter(|candidate| candidate.failure.is_none())
            .map(|candidate| (&mut candidate.tree, false));
        for (tree, active) in active.chain(candidate) {
            let mut changes = metadata::Changes::new();
            for (path, after) in documents.after {
                if !definition.scope().contains(path) {
                    continue;
                }
                let before = documents.before.get(path).and_then(Option::as_ref);
                if let Some(before) = before {
                    budget.charge_decoded(
                        metadata::MAX_INDEX_SCALAR_BYTES + metadata::document_key_len(path),
                    )?;
                    let old = match metadata::index_entry_for_document(
                        definition,
                        path,
                        &before.document,
                    ) {
                        Err(error) if !active && oversized_key(&error) => None,
                        Err(error) if active && oversized_key(&error) => {
                            return Err(corrupt("active document has oversized index key"));
                        }
                        other => other?,
                    };
                    if let Some(key) = old {
                        let location = documents
                            .previous_locations
                            .get(path)
                            .and_then(Option::as_ref)
                            .ok_or_else(|| corrupt("missing previous index location"))?;
                        if existing_entry(snapshot, tree, &key, location, definition, budget)
                            .await?
                            .is_none()
                            && active
                        {
                            return Err(corrupt("active index entry missing"));
                        }
                        changes.insert(key, None);
                    }
                }
                if let Some(after) = after {
                    budget.charge_decoded(
                        metadata::MAX_INDEX_SCALAR_BYTES + metadata::document_key_len(path),
                    )?;
                    if let Some(key) = metadata::index_entry_for_document(definition, path, after)?
                    {
                        let location = documents
                            .locations
                            .get(path)
                            .ok_or_else(|| corrupt("missing new index location"))?;
                        let value = prepare_entry(
                            tree, definition, path, after, location, objects, budget,
                        )?;
                        changes.insert(key, Some(value));
                    }
                }
            }
            let prepared = metadata::prepare(
                snapshot.store.as_ref(),
                Family::ScalarIndexes,
                tree.entries.as_ref(),
                &changes,
                budget,
            )
            .await?;
            changed |= prepared.root != tree.entries;
            tree.entries = prepared.root;
            objects.extend(prepared.objects);
        }
        if changed {
            let value = stage_descriptor(&index.descriptor, objects, budget)?;
            catalog.insert(
                metadata::index_definition_key(index.definition.id()),
                Some(value),
            );
        }
    }
    prepare_catalog(snapshot, root, &catalog, objects, budget).await
}
