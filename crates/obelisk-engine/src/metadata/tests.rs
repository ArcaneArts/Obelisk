use super::codec::Node;
use super::*;
use crate::{Error, format};
use bytes::Bytes;
use obelisk_storage::{ObjectId, Storage};
use obelisk_types::*;
use proptest::prelude::*;
use std::{
    collections::BTreeMap,
    ops::Bound,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, Default)]
struct Memory {
    objects: Mutex<BTreeMap<ObjectId, Bytes>>,
    reads: AtomicUsize,
}
#[async_trait::async_trait]
impl Storage for Memory {
    async fn read(
        &self,
        id: ObjectId,
        range: Option<std::ops::Range<u64>>,
    ) -> obelisk_storage::Result<Bytes> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let objects = self.objects.lock().unwrap();
        let bytes = objects.get(&id).ok_or(obelisk_storage::Error::NotFound)?;
        match range {
            None => Ok(bytes.clone()),
            Some(r) => {
                if r.start > r.end || r.end > bytes.len() as u64 {
                    return Err(obelisk_storage::Error::Invalid("range".into()));
                }
                Ok(bytes.slice(r.start as usize..r.end as usize))
            }
        }
    }
    async fn create(&self, id: ObjectId, bytes: Bytes) -> obelisk_storage::Result<()> {
        let mut objects = self.objects.lock().unwrap();
        if objects.contains_key(&id) {
            return Err(obelisk_storage::Error::Conflict);
        }
        objects.insert(id, bytes);
        Ok(())
    }
    async fn read_head(&self) -> obelisk_storage::Result<Option<obelisk_storage::Head>> {
        Ok(None)
    }
    async fn compare_exchange_head(
        &self,
        _: Option<&obelisk_storage::VersionToken>,
        _: Bytes,
    ) -> obelisk_storage::Result<obelisk_storage::VersionToken> {
        panic!("tree must not publish HEAD")
    }
    async fn list(&self) -> obelisk_storage::Result<Vec<ObjectId>> {
        panic!("tree must not list objects")
    }
    async fn delete(&self, _: ObjectId) -> obelisk_storage::Result<()> {
        panic!("tree must not reclaim objects")
    }
}
async fn apply(store: &Memory, root: Option<&PageRef>, changes: Changes) -> PreparedTree {
    let prepared = prepare(
        store,
        Family::Documents,
        root,
        &changes,
        &mut Budget::default(),
    )
    .await
    .unwrap();
    prepared.persist(store).await.unwrap();
    prepared
}
async fn all(store: &Memory, root: Option<&PageRef>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut after = None::<Vec<u8>>;
    loop {
        let page = range(
            store,
            Family::Documents,
            root,
            KeyRange {
                start: after
                    .as_deref()
                    .map(Bound::Excluded)
                    .unwrap_or(Bound::Unbounded),
                end: None,
            },
            37,
            &mut Budget::default(),
        )
        .await
        .unwrap();
        if page.is_empty() {
            break;
        }
        after = Some(page.last().unwrap().0.clone());
        out.extend(page);
    }
    out
}
async fn assert_balanced(store: &Memory, root: Option<&PageRef>) {
    let mut pending: Vec<_> = root.into_iter().cloned().collect();
    while let Some(page) = pending.pop() {
        let bytes = store.read(page.blob.object, None).await.unwrap();
        let node = Node::decode(&bytes, Family::Documents).unwrap();
        assert!(!node.entries.is_empty());
        if node.level > 0 {
            assert!(
                node.entries.len() >= 2,
                "unary branch at level {}",
                node.level
            );
            for (_, child) in node.entries {
                let child = PageRef::from_bytes(&child).unwrap();
                assert_eq!(child.level + 1, node.level);
                pending.push(child);
            }
        }
    }
}
fn key(i: u64) -> Vec<u8> {
    format!("key-{i:08}").into_bytes()
}

#[tokio::test]
async fn generated_batches_match_reference_and_preserve_old_roots() {
    let store = Memory::default();
    let mut root = None;
    let mut expected = BTreeMap::new();
    let mut rng = 9_u64;
    let mut pinned = None;
    let mut pinned_values = Vec::new();
    for round in 0..60 {
        let mut changes = Changes::new();
        for _ in 0..80 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let k = key((rng >> 24) % 900);
            let value = if rng.is_multiple_of(5) {
                None
            } else {
                Some(vec![(round + 1) as u8; 300])
            };
            changes.insert(k, value);
        }
        for (k, v) in &changes {
            if let Some(v) = v {
                expected.insert(k.clone(), v.clone());
            } else {
                expected.remove(k);
            }
        }
        root = apply(&store, root.as_ref(), changes).await.root;
        assert_balanced(&store, root.as_ref()).await;
        if round % 10 == 0 {
            assert_eq!(
                all(&store, root.as_ref()).await,
                expected.clone().into_iter().collect::<Vec<_>>()
            );
        }
        if round == 20 {
            pinned = root.clone();
            pinned_values = all(&store, pinned.as_ref()).await;
        }
        let k = key(round);
        assert_eq!(
            get(
                &store,
                Family::Documents,
                root.as_ref(),
                &k,
                &mut Budget::default()
            )
            .await
            .unwrap(),
            expected.get(&k).cloned()
        );
    }
    assert_eq!(all(&store, pinned.as_ref()).await, pinned_values);
    let removed: Changes = expected.keys().map(|k| (k.clone(), None)).collect();
    let empty = apply(&store, root.as_ref(), removed).await;
    assert!(empty.root.is_none());
    assert!(empty.objects.is_empty());
}

#[tokio::test]
async fn local_updates_reuse_subtrees_and_noops_write_nothing() {
    let store = Memory::default();
    let original = apply(
        &store,
        None,
        (0..1500).map(|i| (key(i), Some(vec![1; 400]))).collect(),
    )
    .await;
    let root = original.root.as_ref().unwrap();
    assert!(root.level > 0);
    let bytes = store.read(root.blob.object, None).await.unwrap();
    let old = Node::decode(&bytes, Family::Documents).unwrap();
    let changed = apply(
        &store,
        Some(root),
        BTreeMap::from([(key(700), Some(vec![2; 400]))]),
    )
    .await;
    assert!(changed.objects.len() <= 3 * (root.level as usize + 1));
    let newroot = changed.root.as_ref().unwrap();
    let bytes = store.read(newroot.blob.object, None).await.unwrap();
    let new = Node::decode(&bytes, Family::Documents).unwrap();
    assert!(
        old.entries
            .iter()
            .filter(|e| new.entries.contains(e))
            .count()
            >= old.entries.len() - 2
    );
    let noop = apply(
        &store,
        Some(newroot),
        BTreeMap::from([(key(700), Some(vec![2; 400])), (key(9999), None)]),
    )
    .await;
    assert_eq!(noop.root, changed.root);
    assert!(noop.objects.is_empty());
    let start = key(699);
    let end = key(702);
    let page = range(
        &store,
        Family::Documents,
        Some(newroot),
        KeyRange {
            start: Bound::Included(&start),
            end: Some(&end),
        },
        10,
        &mut Budget::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        page.iter().map(|e| e.0.clone()).collect::<Vec<_>>(),
        vec![key(699), key(700), key(701)]
    );
}

#[tokio::test]
async fn maximum_paths_split_multiple_levels_and_collapse_after_deletion() {
    let store = Memory::default();
    let mut changes = Changes::new();
    for i in 0..256 {
        let path =
            DocumentPath::from_segments([format!("{i:04}{}", "z".repeat(MAX_PATH_BYTES - 4))])
                .unwrap();
        changes.insert(document_key(&path), Some(vec![1; 100]));
    }
    let first = apply(&store, None, changes.clone()).await;
    assert!(first.root.as_ref().unwrap().level >= 2);
    assert_balanced(&store, first.root.as_ref()).await;
    assert_eq!(all(&store, first.root.as_ref()).await.len(), 256);
    let last = changes.last_key_value().unwrap().0.clone();
    let delete = changes
        .keys()
        .filter(|k| **k != last)
        .map(|k| (k.clone(), None))
        .collect();
    let single = apply(&store, first.root.as_ref(), delete).await;
    assert_eq!(single.root.as_ref().unwrap().level, 0);
    assert_eq!(
        all(&store, single.root.as_ref()).await,
        vec![(last, vec![1; 100])]
    );
    assert_eq!(all(&store, first.root.as_ref()).await.len(), 256);
}

#[test]
fn ordered_paths_and_memberships_respect_boundaries() {
    let mut deepest = vec!["x".repeat(31); 128];
    deepest.last_mut().unwrap().push('x');
    let paths = vec![
        DocumentPath::parse("a").unwrap(),
        DocumentPath::parse("a/b").unwrap(),
        DocumentPath::parse("a/b/c").unwrap(),
        DocumentPath::parse("a/bc").unwrap(),
        DocumentPath::from_segments(["a/b"]).unwrap(),
        DocumentPath::parse("é/🙂").unwrap(),
        DocumentPath::from_segments(["x".repeat(MAX_PATH_BYTES)]).unwrap(),
        DocumentPath::from_segments(["/".repeat(MAX_PATH_BYTES / 2)]).unwrap(),
        DocumentPath::from_segments(["\\".repeat(MAX_PATH_BYTES / 2)]).unwrap(),
        DocumentPath::from_segments(["é".repeat(MAX_PATH_BYTES / 2)]).unwrap(),
        DocumentPath::from_segments(deepest).unwrap(),
    ];
    for path in &paths {
        assert_eq!(document_path(&document_key(path)).unwrap(), *path);
        assert_eq!(membership_path(&membership_key(path)).unwrap(), *path);
    }
    let mut by_path = paths.clone();
    by_path.sort();
    let mut by_key = paths;
    by_key.sort_by_key(document_key);
    assert_eq!(by_path, by_key);
    let parent = DocumentPath::parse("a/b").unwrap();
    let prefix = document_key(&parent);
    let end = prefix_end(&prefix).unwrap();
    assert!(document_key(&DocumentPath::parse("a/b/c").unwrap()) < end);
    assert!(document_key(&DocumentPath::parse("a/bc").unwrap()) >= end);
    assert!(
        membership_key(&DocumentPath::parse("a/b/c").unwrap())
            .starts_with(&parent_prefix(Some(&parent)))
    );
    assert!(
        !membership_key(&DocumentPath::parse("a/bc/c").unwrap())
            .starts_with(&parent_prefix(Some(&parent)))
    );
    assert!(membership_key(&DocumentPath::parse("a").unwrap()).starts_with(&parent_prefix(None)));
}

#[test]
fn roots_and_large_outcomes_roundtrip_without_page_overflow() {
    let receipt = CommitReceipt {
        database_id: DatabaseId::new("db").unwrap(),
        request_id: RequestId::new("request").unwrap(),
        commit_id: CommitId::new("commit").unwrap(),
        sequence: 1,
        revisions: (0..256)
            .map(|i| {
                (
                    DocumentPath::parse(&format!("{i}{}", "x".repeat(300))).unwrap(),
                    Some(Revision::new("commit").unwrap()),
                )
            })
            .collect(),
    };
    let record = format::RequestRecord {
        fingerprint: [9; 32],
        receipt,
    };
    let bytes = encode_outcome(&record).unwrap();
    assert!(bytes.len() > MAX_PAGE_BYTES);
    assert_eq!(decode_outcome(&bytes).unwrap().receipt, record.receipt);
    let root = Root {
        database_id: record.receipt.database_id.clone(),
        commit_id: record.receipt.commit_id.clone(),
        sequence: 1,
        documents: None,
        requests: None,
        membership: None,
    };
    let bytes = encode_root(&root).unwrap();
    assert!(bytes.len() < 1024);
    assert_eq!(decode_root(&bytes).unwrap().sequence, 1);
    assert!(matches!(
        format::decode_manifest(&bytes),
        Err(Error::UnsupportedVersion(2))
    ));
}

#[tokio::test]
async fn hostile_pages_descriptors_and_budgets_fail_closed() {
    let store = Memory::default();
    let prepared = apply(&store, None, BTreeMap::from([(key(1), Some(vec![7; 5]))])).await;
    let root = prepared.root.unwrap();
    let before = store.reads.load(Ordering::Relaxed);
    let mut budget = Budget {
        decoded: 1,
        ..Budget::default()
    };
    assert!(matches!(
        get(&store, Family::Documents, Some(&root), &key(1), &mut budget).await,
        Err(Error::ResourceLimit(_))
    ));
    assert_eq!(store.reads.load(Ordering::Relaxed), before);
    let mut budget = Budget {
        page_reads: 0,
        ..Budget::default()
    };
    assert!(
        get(&store, Family::Documents, Some(&root), &key(1), &mut budget)
            .await
            .is_err()
    );
    assert!(matches!(
        get(
            &store,
            Family::Requests,
            Some(&root),
            &key(1),
            &mut Budget::default()
        )
        .await,
        Err(Error::Corrupt(_))
    ));
    let mut bad = root.clone();
    bad.count += 1;
    assert!(
        get(
            &store,
            Family::Documents,
            Some(&bad),
            &key(1),
            &mut Budget::default()
        )
        .await
        .is_err()
    );
    let mut bad = root.clone();
    bad.blob.len = u32::MAX;
    assert!(
        get(
            &store,
            Family::Documents,
            Some(&bad),
            &key(1),
            &mut Budget::default()
        )
        .await
        .is_err()
    );
    let node = Node {
        level: 1,
        entries: vec![(b"wrong-minimum".to_vec(), root.bytes())],
    };
    let bytes = node.encode(Family::Documents).unwrap();
    let id = ObjectId::random();
    store.create(id, bytes.clone().into()).await.unwrap();
    let bad = PageRef {
        blob: ObjectRef::new(id, &bytes),
        level: 1,
        count: 1,
    };
    assert!(
        get(
            &store,
            Family::Documents,
            Some(&bad),
            b"zz",
            &mut Budget::default()
        )
        .await
        .is_err()
    );
    let mut bytes = store.read(root.blob.object, None).await.unwrap().to_vec();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    store
        .objects
        .lock()
        .unwrap()
        .insert(root.blob.object, bytes.into());
    assert!(
        get(
            &store,
            Family::Documents,
            Some(&root),
            &key(1),
            &mut Budget::default()
        )
        .await
        .is_err()
    );
    assert!(
        Node {
            level: 0,
            entries: vec![(vec![1], vec![0; MAX_VALUE_BYTES + 1])]
        }
        .encode(Family::Documents)
        .is_err()
    );
}

proptest! {
    #[test]
    fn key_order_matches_structured_order(a in prop::collection::vec("[a-zA-Z./\\\\é🙂]{1,12}",1..8), b in prop::collection::vec("[a-zA-Z./\\\\é🙂]{1,12}",1..8)) {
        let a=DocumentPath::from_segments(a).unwrap(); let b=DocumentPath::from_segments(b).unwrap();
        prop_assert_eq!(a.cmp(&b),document_key(&a).cmp(&document_key(&b)));
        prop_assert_eq!(document_path(&document_key(&a)).unwrap(),a.clone());
        prop_assert_eq!(membership_path(&membership_key(&a)).unwrap(),a);
    }
    #[test]
    fn arbitrary_and_checksummed_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(),0..4096)) {
        let _=decode_root(&bytes);let _=decode_outcome(&bytes);let _=Node::decode(&bytes,Family::Documents); let _=document_path(&bytes);let _=membership_path(&bytes);
        let page=format::envelope(PAGE_KIND,bytes.clone()).unwrap();let _=Node::decode(&page,Family::Documents);
        let root=format::envelope_version(2,2,bytes.clone()).unwrap();let _=decode_root(&root);
        let outcome=format::envelope(OUTCOME_KIND,bytes).unwrap();let _=decode_outcome(&outcome);
    }
}

#[tokio::test]
async fn shuffled_wide_path_deletions_keep_internal_fanout() {
    let store = Memory::default();
    let keys: Vec<_> = (0..256)
        .map(|i| {
            document_key(
                &DocumentPath::from_segments([format!("{i:04}{}", "w".repeat(4000))]).unwrap(),
            )
        })
        .collect();
    let mut root = apply(
        &store,
        None,
        keys.iter()
            .map(|k| (k.clone(), Some(vec![1; 100])))
            .collect(),
    )
    .await
    .root;
    for batch in 0..15 {
        let changes = (batch * 16..batch * 16 + 16)
            .map(|i| (keys[(i * 73) % 256].clone(), None))
            .collect();
        root = apply(&store, root.as_ref(), changes).await.root;
        assert_balanced(&store, root.as_ref()).await;
        assert_eq!(
            all(&store, root.as_ref()).await.len(),
            256 - (batch + 1) * 16
        );
    }
}

#[tokio::test]
async fn checksummed_metadata_rejects_semantic_corruption_and_missing_children() {
    let mut bytes = Node {
        level: 0,
        entries: vec![(b"a".to_vec(), vec![1])],
    }
    .encode(Family::Documents)
    .unwrap();
    bytes[5..7].copy_from_slice(&2_u16.to_le_bytes());
    assert!(matches!(
        Node::decode(&bytes, Family::Documents),
        Err(Error::UnsupportedVersion(2))
    ));
    let mut writer = format::Writer::default();
    writer.byte(Family::Documents as u8);
    writer.byte(0);
    writer.u32(2);
    for value in [b"first".as_slice(), b"second".as_slice()] {
        writer.bytes(b"duplicate");
        writer.bytes(value);
    }
    let bytes = format::envelope(PAGE_KIND, writer.0).unwrap();
    assert!(matches!(
        Node::decode(&bytes, Family::Documents),
        Err(Error::Corrupt(_))
    ));
    let store = Memory::default();
    let tree = apply(&store, None, BTreeMap::from([(key(1), Some(vec![1]))])).await;
    let mut missing = tree.root.unwrap();
    missing.blob.object = ObjectId::random();
    assert!(matches!(
        get(
            &store,
            Family::Documents,
            Some(&missing),
            &key(1),
            &mut Budget::default()
        )
        .await,
        Err(Error::Corrupt(_))
    ));
    let child = PageRef {
        blob: ObjectRef {
            object: ObjectId::random(),
            len: 100,
            hash: [0; 32],
        },
        level: 31,
        count: 1,
    };
    assert!(
        Node {
            level: 1,
            entries: vec![(b"a".to_vec(), child.bytes())]
        }
        .encode(Family::Documents)
        .is_err()
    );
    let mut writer = format::Writer::default();
    writer.byte(Family::Documents as u8);
    writer.byte(0);
    writer.u32(usize::MAX);
    let bytes = format::envelope(PAGE_KIND, writer.0).unwrap();
    assert!(Node::decode(&bytes, Family::Documents).is_err());
}

#[tokio::test]
async fn a_local_edit_preserves_a_distant_sparse_tail() {
    let store = Memory::default();
    let tree = apply(
        &store,
        None,
        (0..1141).map(|i| (key(i), Some(vec![1; 400]))).collect(),
    )
    .await;
    let root = tree.root.as_ref().unwrap();
    assert_eq!(root.level, 1);
    let original = Node::decode(
        &store.read(root.blob.object, None).await.unwrap(),
        Family::Documents,
    )
    .unwrap();
    let tail = PageRef::from_bytes(&original.entries.last().unwrap().1).unwrap();
    assert!((tail.blob.len as usize) < PAGE_TARGET / 3);
    let edited = apply(
        &store,
        Some(root),
        BTreeMap::from([(key(0), Some(vec![2; 400]))]),
    )
    .await;
    assert_eq!(
        edited.objects.len(),
        2,
        "only the changed leaf and root should be created"
    );
    let updated = Node::decode(
        &store
            .read(edited.root.as_ref().unwrap().blob.object, None)
            .await
            .unwrap(),
        Family::Documents,
    )
    .unwrap();
    assert_eq!(&updated.entries[1..], &original.entries[1..]);
    let unchanged = apply(
        &store,
        edited.root.as_ref(),
        BTreeMap::from([(key(0), Some(vec![2; 400]))]),
    )
    .await;
    assert!(unchanged.objects.is_empty());
}

#[tokio::test]
async fn cyclic_references_fail_with_bounded_point_range_and_edit_work() {
    let store = Memory::default();
    let object = ObjectId::random();
    // A child points back to its own branch object, claiming a lower level.
    // Its hash cannot authenticate the self-referential bytes. The outer
    // reference and envelope are valid so traversal must reach the bad edge.
    let mut child = PageRef {
        blob: ObjectRef {
            object,
            len: 100,
            hash: [0; 32],
        },
        level: 0,
        count: 1,
    };
    let encode = |child: &PageRef| {
        Node {
            level: 1,
            entries: vec![(b"a".to_vec(), child.bytes())],
        }
        .encode(Family::Documents)
        .unwrap()
    };
    child.blob.len = encode(&child).len() as u32;
    let bytes = encode(&child);
    assert_eq!(bytes.len(), child.blob.len as usize);
    let root = PageRef {
        blob: ObjectRef::new(object, &bytes),
        level: 1,
        count: 1,
    };
    store.create(object, bytes.into()).await.unwrap();
    for operation in 0..3 {
        let before = store.reads.load(Ordering::Relaxed);
        let mut budget = Budget::default();
        let result = match operation {
            0 => get(&store, Family::Documents, Some(&root), b"a", &mut budget)
                .await
                .map(|_| ()),
            1 => range(
                &store,
                Family::Documents,
                Some(&root),
                KeyRange {
                    start: Bound::Unbounded,
                    end: None,
                },
                1,
                &mut budget,
            )
            .await
            .map(|_| ()),
            _ => prepare(
                &store,
                Family::Documents,
                Some(&root),
                &BTreeMap::from([(b"a".to_vec(), Some(vec![1]))]),
                &mut budget,
            )
            .await
            .map(|_| ()),
        };
        assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
        // The editor rejects the inconsistent cached descriptor; uncached
        // readers reject the child hash after reading the branch a second time.
        let expected_reads = if operation == 2 { 1 } else { 2 };
        assert_eq!(store.reads.load(Ordering::Relaxed) - before, expected_reads);
        assert_eq!(store.objects.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn non_descending_child_levels_and_transfer_limits_fail_before_following_edges() {
    let store = Memory::default();
    let child = PageRef {
        blob: ObjectRef {
            object: ObjectId::random(),
            len: 100,
            hash: [0; 32],
        },
        level: 1,
        count: 1,
    };
    // Bypass the writer's validation to present a checksummed hostile branch
    // with a same-level child to the production reader.
    let mut writer = format::Writer::default();
    writer.byte(Family::Documents as u8);
    writer.byte(1);
    writer.u32(1);
    writer.bytes(b"a");
    writer.bytes(&child.bytes());
    let bytes = format::envelope(PAGE_KIND, writer.0).unwrap();
    let object = ObjectId::random();
    let root = PageRef {
        blob: ObjectRef::new(object, &bytes),
        level: 1,
        count: 1,
    };
    store.create(object, bytes.into()).await.unwrap();
    let before = store.reads.load(Ordering::Relaxed);
    let result = get(
        &store,
        Family::Documents,
        Some(&root),
        b"a",
        &mut Budget::default(),
    )
    .await;
    assert!(matches!(result, Err(Error::Corrupt(_))), "{result:?}");
    assert_eq!(store.reads.load(Ordering::Relaxed) - before, 1);

    let before = store.reads.load(Ordering::Relaxed);
    let mut budget = Budget {
        transferred: root.blob.len as usize - 1,
        ..Budget::default()
    };
    let result = get(&store, Family::Documents, Some(&root), b"a", &mut budget).await;
    assert!(matches!(
        result,
        Err(Error::ResourceLimit("metadata transfer bytes"))
    ));
    assert_eq!(store.reads.load(Ordering::Relaxed), before);
}
