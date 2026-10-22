use crate::{
    Engine, Error, Result, Snapshot, SnapshotReference, corrupt, format,
    metadata::{self, Budget, Family, KeyRange},
};
use obelisk_storage::VersionToken;
use obelisk_types::*;
use std::{collections::BTreeMap, ops::Bound};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Children,
    Descendants,
}
/// Opaque Rust continuation obtained from a committed snapshot. No wire decoder.
#[derive(Debug, Clone)]
pub struct QueryCursor {
    snapshot: SnapshotReference,
    scope: QueryScope,
    kind: Kind,
    version: u16,
    last_key: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct QueryPage {
    pub snapshot: SnapshotReference,
    pub entries: Vec<QueryEntry>,
    pub next: Option<QueryCursor>,
}
fn validate_cursor(cursor: Option<&QueryCursor>, scope: &QueryScope, kind: Kind) -> Result<()> {
    if cursor.is_some_and(|c| c.version != 1 || &c.scope != scope || c.kind != kind) {
        return Err(Error::InvalidCursor("query mismatch"));
    }
    Ok(())
}
fn scope_contains(scope: &QueryScope, kind: Kind, path: &DocumentPath) -> bool {
    match (scope, kind) {
        (QueryScope::Root, Kind::Children) => path.segments().len() == 1,
        (QueryScope::Root, Kind::Descendants) => true,
        (QueryScope::Path(parent), Kind::Children) => {
            parent.is_ancestor_of(path) && path.segments().len() == parent.segments().len() + 1
        }
        (QueryScope::Path(parent), Kind::Descendants) => parent.is_ancestor_of(path),
    }
}
fn result_bytes(path: &DocumentPath, doc: &VersionedDocument) -> Result<usize> {
    Ok(format::HEADER_BYTES
        + 5
        + path.segments().iter().map(|p| p.len() + 4).sum::<usize>()
        + doc.document.estimated_size()?
        + 4
        + doc.revision.as_str().len())
}
impl Engine {
    pub async fn get_many(&self, paths: &[DocumentPath]) -> Result<Vec<Option<VersionedDocument>>> {
        if paths.len() > MAX_QUERY_ITEMS {
            return Err(Error::ResourceLimit("bulk path count"));
        }
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut budget = self.options.resources.budget();
        let snapshot = self.snapshot_with_budget(&mut budget).await?;
        snapshot.get_many_with_budget(paths, &mut budget).await
    }
    pub async fn children(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.query(scope, Kind::Children, options, cursor).await
    }
    pub async fn descendants(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.query(scope, Kind::Descendants, options, cursor).await
    }
    async fn query(
        &self,
        scope: &QueryScope,
        kind: Kind,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        validate_cursor(cursor, scope, kind)?;
        let mut budget = self.options.resources.budget();
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
            .query(scope, kind, options, cursor, &mut budget)
            .await
    }
}
impl Snapshot {
    pub async fn get_many(&self, paths: &[DocumentPath]) -> Result<Vec<Option<VersionedDocument>>> {
        self.get_many_with_budget(paths, &mut self.resources.budget())
            .await
    }
    async fn get_many_with_budget(
        &self,
        paths: &[DocumentPath],
        budget: &mut Budget,
    ) -> Result<Vec<Option<VersionedDocument>>> {
        if paths.len() > MAX_QUERY_ITEMS {
            return Err(Error::ResourceLimit("bulk path count"));
        }
        budget.charge_decoded(paths.len() * std::mem::size_of::<Option<VersionedDocument>>())?;
        let mut cached = BTreeMap::<DocumentPath, Option<VersionedDocument>>::new();
        let mut out = Vec::with_capacity(paths.len());
        let mut bytes = 0;
        for path in paths {
            if !cached.contains_key(path) {
                budget.charge_decoded(
                    128 + path.segments().iter().map(|s| s.len() + 64).sum::<usize>(),
                )?;
                cached.insert(path.clone(), self.get_with_budget(path, budget).await?);
            }
            let value = &cached[path];
            let size = value
                .as_ref()
                .map(|d| result_bytes(path, d))
                .transpose()?
                .unwrap_or(1);
            bytes += size;
            if bytes > MAX_QUERY_BYTES {
                return Err(Error::ResourceLimit("bulk response bytes"));
            }
            if let Some(doc) = value {
                budget.charge_decoded(
                    format::document_allocation(&doc.document) + doc.revision.as_str().len() + 64,
                )?;
            }
            out.push(value.clone());
        }
        Ok(out)
    }
    pub async fn children(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.query(
            scope,
            Kind::Children,
            options,
            cursor,
            &mut self.resources.budget(),
        )
        .await
    }
    pub async fn descendants(
        &self,
        scope: &QueryScope,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
    ) -> Result<QueryPage> {
        self.query(
            scope,
            Kind::Descendants,
            options,
            cursor,
            &mut self.resources.budget(),
        )
        .await
    }
    async fn query(
        &self,
        scope: &QueryScope,
        kind: Kind,
        options: PageOptions,
        cursor: Option<&QueryCursor>,
        budget: &mut Budget,
    ) -> Result<QueryPage> {
        validate_cursor(cursor, scope, kind)?;
        if cursor.is_some_and(|c| c.snapshot != self.reference()) {
            return Err(Error::InvalidCursor("snapshot mismatch"));
        }
        let (family, root, prefix) = match kind {
            Kind::Children => (
                Family::Membership,
                self.manifest.membership.as_ref(),
                metadata::parent_prefix(match scope {
                    QueryScope::Root => None,
                    QueryScope::Path(p) => Some(p),
                }),
            ),
            Kind::Descendants => (
                Family::Documents,
                self.manifest.documents.as_ref(),
                match scope {
                    QueryScope::Root => Vec::new(),
                    QueryScope::Path(p) => metadata::document_key(p),
                },
            ),
        };
        if let Some(cursor) = cursor {
            budget.charge_decoded(
                cursor.last_key.len()
                    + cursor.last_key.iter().filter(|b| **b == 0).count() * 64
                    + 128,
            )?;
            let position = match kind {
                Kind::Children => metadata::membership_path(&cursor.last_key),
                Kind::Descendants => metadata::document_path(&cursor.last_key),
            }
            .map_err(|_| Error::InvalidCursor("invalid position key"))?;
            if !scope_contains(scope, kind, &position) {
                return Err(Error::InvalidCursor("position outside scope"));
            }
        }
        let end = metadata::prefix_end(&prefix);
        if cursor.is_some_and(|c| {
            !c.last_key.starts_with(&prefix) || end.as_ref().is_some_and(|end| &c.last_key >= end)
        }) {
            return Err(Error::InvalidCursor("position outside scope"));
        }
        let start = if let Some(cursor) = cursor {
            Bound::Excluded(cursor.last_key.as_slice())
        } else if kind == Kind::Descendants && !prefix.is_empty() {
            Bound::Excluded(prefix.as_slice())
        } else {
            Bound::Included(prefix.as_slice())
        };
        let rows = metadata::range(
            self.store.as_ref(),
            family,
            root,
            KeyRange {
                start,
                end: end.as_deref(),
            },
            options.limit() + 1,
            budget,
        )
        .await?;
        let total = rows.len();
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut last_key = None;
        for (key, value) in rows.into_iter().take(options.limit()) {
            budget
                .charge_decoded(key.len() + key.iter().filter(|b| **b == 0).count() * 64 + 128)?;
            let path = match kind {
                Kind::Children => metadata::membership_path(&key)?,
                Kind::Descendants => metadata::document_path(&key)?,
            };
            if !scope_contains(scope, kind, &path) {
                return Err(corrupt("hierarchy key outside scope"));
            }
            let location = match kind {
                Kind::Descendants => metadata::decode_location(&value)?,
                Kind::Children => {
                    if !value.is_empty() {
                        return Err(corrupt("invalid membership marker"));
                    }
                    self.location(&path, budget)
                        .await?
                        .ok_or_else(|| corrupt("membership points to missing document"))?
                }
            };
            let cost = (location.range.end - location.range.start) as usize
                + 4
                + location.revision.as_str().len();
            if bytes + cost > options.max_bytes() {
                if entries.is_empty() {
                    return Err(Error::ResourceLimit("document exceeds query page bytes"));
                }
                break;
            }
            let document = self
                .read_location(&path, location, budget)
                .await?
                .ok_or_else(|| corrupt("missing query document"))?;
            budget.charge_decoded(std::mem::size_of::<QueryEntry>() + key.len())?;
            entries.push(QueryEntry {
                path,
                document: document.document,
                revision: document.revision,
            });
            bytes += cost;
            last_key = Some(key);
        }
        budget.charge_decoded(1024)?; // Owned snapshot identity in the result.
        if total > entries.len() {
            let scope_bytes = match scope {
                QueryScope::Root => 0,
                QueryScope::Path(p) => p.segments().iter().map(|s| s.len() + 64).sum(),
            };
            budget.charge_decoded(1152 + scope_bytes)?;
        }
        let next = if total > entries.len() {
            Some(QueryCursor {
                snapshot: self.reference(),
                scope: scope.clone(),
                kind,
                version: 1,
                last_key: last_key.ok_or_else(|| corrupt("query failed to make progress"))?,
            })
        } else {
            None
        };
        Ok(QueryPage {
            snapshot: self.reference(),
            entries,
            next,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn malformed_native_cursor_versions_and_positions_fail_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(
            Arc::new(
                obelisk_storage::folder::Folder::open(dir.path())
                    .await
                    .unwrap(),
            ),
            crate::EngineOptions::default(),
        )
        .await
        .unwrap();
        let mutations = ["a/1", "a/2"]
            .into_iter()
            .map(|p| Mutation {
                path: DocumentPath::parse(p).unwrap(),
                condition: Condition::Any,
                kind: MutationKind::Put(Document::default()),
            })
            .collect();
        engine
            .commit(&Transaction::new(RequestId::new("seed").unwrap(), mutations, vec![]).unwrap())
            .await
            .unwrap();
        let scope = QueryScope::path("a").unwrap();
        let options = PageOptions::new(1).unwrap();
        let cursor = engine
            .children(&scope, options, None)
            .await
            .unwrap()
            .next
            .unwrap();
        let mut invalid = cursor.clone();
        invalid.version = 2;
        assert!(matches!(
            engine.children(&scope, options, Some(&invalid)).await,
            Err(Error::InvalidCursor(_))
        ));
        for key in [
            vec![],
            vec![255],
            metadata::membership_key(&DocumentPath::parse("other/1").unwrap()),
        ] {
            let mut invalid = cursor.clone();
            invalid.last_key = key;
            assert!(matches!(
                engine.children(&scope, options, Some(&invalid)).await,
                Err(Error::InvalidCursor(_))
            ));
        }
    }
}
