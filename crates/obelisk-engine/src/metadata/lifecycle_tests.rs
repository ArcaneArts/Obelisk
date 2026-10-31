use super::{lifecycle::*, *};
use crate::{Error, format};
use obelisk_storage::ObjectId;
use obelisk_types::*;
use proptest::prelude::*;
use std::collections::BTreeMap;

fn budget() -> usize {
    64 * 1024 * 1024
}
fn path(s: &str) -> DocumentPath {
    DocumentPath::parse(s).unwrap()
}
fn generation(s: &str) -> IndexGeneration {
    IndexGeneration::new(s).unwrap()
}
fn definition() -> IndexDefinition {
    IndexDefinition::new(
        IndexId::new("people-by-name").unwrap(),
        IndexScope::Descendants(QueryScope::path("people").unwrap()),
        vec![IndexField::new(
            FieldPath::parse("name").unwrap(),
            ScalarType::String,
            IndexOrder::Ascending,
        )],
        vec![FieldPath::parse("avatar").unwrap()],
    )
    .unwrap()
}
fn object(bytes: &[u8]) -> ObjectRef {
    ObjectRef::new(ObjectId::random(), bytes)
}
fn root() -> RootV3 {
    RootV3 {
        data: Root {
            database_id: DatabaseId::new("database").unwrap(),
            commit_id: CommitId::new("commit").unwrap(),
            sequence: 12,
            documents: None,
            requests: None,
            membership: None,
        },
        catalog: None,
        index_count: 0,
    }
}
fn head() -> HeadRef {
    let root = root();
    HeadRef {
        database_id: root.data.database_id.clone(),
        commit_id: root.data.commit_id.clone(),
        sequence: root.data.sequence,
        root: object(&encode_root_v3(&root).unwrap()),
    }
}
fn descriptor() -> IndexDescriptor {
    IndexDescriptor {
        database_id: root().data.database_id,
        id: definition().id().clone(),
        definition: object(&encode_index_definition(&definition()).unwrap()),
        active: None,
        candidate: Some(IndexCandidate {
            tree: IndexGenerationRoot {
                generation: generation("candidate"),
                entries: None,
            },
            base: head(),
            progress: IndexBuildProgress::default(),
            failure: None,
        }),
    }
}
fn location() -> format::Location {
    format::Location {
        object: ObjectId::random(),
        range: 64..512,
        revision: Revision::new("revision").unwrap(),
    }
}
fn receipt(operation: IndexOperation, state: IndexState) -> IndexReceipt {
    IndexReceipt {
        publication: CommitReceipt {
            database_id: root().data.database_id,
            request_id: RequestId::new("request").unwrap(),
            commit_id: CommitId::new("management-commit").unwrap(),
            sequence: 14,
            revisions: BTreeMap::new(),
        },
        handle: IndexHandle::new(definition().id().clone(), generation("candidate")),
        operation,
        state,
        progress: IndexBuildProgress::default(),
        failure: None,
    }
}

#[test]
fn v3_heads_roots_are_additive_and_reject_inconsistent_catalogs() {
    let original = root();
    let bytes = encode_root_v3(&original).unwrap();
    assert_eq!(decode_root_v3(&bytes, &mut budget()).unwrap(), original);
    assert!(matches!(
        decode_root(&bytes),
        Err(Error::UnsupportedVersion(3))
    ));
    let old = encode_root(&original.data).unwrap();
    assert!(matches!(
        decode_root_v3(&old, &mut budget()),
        Err(Error::UnsupportedVersion(2))
    ));
    let original = head();
    let bytes = encode_head_v3(&original).unwrap();
    assert_eq!(decode_head_v3(&bytes, &mut budget()).unwrap(), original);
    assert!(matches!(
        decode_head(&bytes),
        Err(Error::UnsupportedVersion(3))
    ));
    assert!(matches!(
        decode_head_v3(&encode_head(&original).unwrap(), &mut budget()),
        Err(Error::UnsupportedVersion(2))
    ));
    let mut invalid = root();
    invalid.index_count = 1;
    assert!(encode_root_v3(&invalid).is_err());
    invalid.catalog = Some(PageRef {
        blob: object(&[0; 50]),
        level: 0,
        count: 1,
    });
    assert_eq!(
        decode_root_v3(&encode_root_v3(&invalid).unwrap(), &mut budget()).unwrap(),
        invalid
    );
    invalid.index_count = 0;
    assert!(encode_root_v3(&invalid).is_err());
    invalid.index_count = MAX_LIVE_INDEXES + 1;
    assert!(encode_root_v3(&invalid).is_err());
    invalid.index_count = usize::MAX;
    assert!(encode_root_v3(&invalid).is_err());
    // Valid checksum with an unknown required capability must still fail.
    let encoded = encode_root_v3(&root()).unwrap();
    let mut payload = format::payload_version(&encoded, 2, 3).unwrap().to_vec();
    let caps = 4 + "database".len() + 4 + "commit".len() + 8;
    payload[caps..caps + 4].copy_from_slice(&7_u32.to_le_bytes());
    let corrupt = format::envelope_version(2, 3, payload).unwrap();
    assert!(decode_root_v3(&corrupt, &mut budget()).is_err());
}

#[test]
fn descriptors_cover_build_rebuild_failure_and_scope_validation() {
    let mut original = descriptor();
    for failure in [
        None,
        Some(IndexBuildFailure::ScalarKeyTooLarge(path("people/a"))),
    ] {
        original.candidate.as_mut().unwrap().failure = failure;
        original.candidate.as_mut().unwrap().progress =
            IndexBuildProgress::new(7, Some(path("people/a"))).unwrap();
        let bytes = encode_index_descriptor(&original).unwrap();
        assert!(bytes.len() <= MAX_DESCRIPTOR_BYTES);
        assert_eq!(
            decode_index_descriptor(&bytes, &mut budget()).unwrap(),
            original
        );
        validate_descriptor_definition(&original, &definition()).unwrap();
    }
    original.active = Some(IndexGenerationRoot {
        generation: generation("active"),
        entries: None,
    });
    assert_eq!(
        decode_index_descriptor(&encode_index_descriptor(&original).unwrap(), &mut budget())
            .unwrap(),
        original
    );
    original.candidate.as_mut().unwrap().tree.generation = generation("active");
    assert!(encode_index_descriptor(&original).is_err());
    original.candidate = None;
    assert_eq!(
        decode_index_descriptor(&encode_index_descriptor(&original).unwrap(), &mut budget())
            .unwrap(),
        original
    );
    original.active = None;
    assert!(encode_index_descriptor(&original).is_err());
    let mut invalid = descriptor();
    invalid.candidate.as_mut().unwrap().base.database_id = DatabaseId::new("other").unwrap();
    assert!(encode_index_descriptor(&invalid).is_err());
    let mut invalid = descriptor();
    invalid.candidate.as_mut().unwrap().progress =
        IndexBuildProgress::new(1, Some(path("elsewhere"))).unwrap();
    assert!(validate_descriptor_definition(&invalid, &definition()).is_err());
    let mut invalid = descriptor();
    invalid.id = IndexId::new("other").unwrap();
    assert!(validate_descriptor_definition(&invalid, &definition()).is_err());
}

#[test]
fn maximum_paths_and_identities_fit_bounded_descriptors() {
    let maximum = DocumentPath::from_segments(vec!["x".repeat(31); 128]).unwrap();
    let mut original = descriptor();
    original.id = IndexId::new("i".repeat(256)).unwrap();
    original.database_id = DatabaseId::new("d".repeat(256)).unwrap();
    let candidate = original.candidate.as_mut().unwrap();
    candidate.base.database_id = original.database_id.clone();
    candidate.tree.generation = generation(&"g".repeat(256));
    candidate.progress = IndexBuildProgress::new(u64::MAX, Some(maximum.clone())).unwrap();
    candidate.failure = Some(IndexBuildFailure::ScalarKeyTooLarge(maximum));
    original.active = Some(IndexGenerationRoot {
        generation: generation(&"a".repeat(256)),
        entries: None,
    });
    let bytes = encode_index_descriptor(&original).unwrap();
    assert!(bytes.len() < MAX_DESCRIPTOR_BYTES);
    assert_eq!(
        decode_index_descriptor(&bytes, &mut budget()).unwrap(),
        original
    );
}

#[test]
fn projection_objects_preserve_large_binary_and_typed_lists_outside_leaves() {
    let projection = CoveredProjection {
        generation: generation("candidate"),
        path: path("people/a\\/b"),
        revision: Revision::new("revision").unwrap(),
        document: Document::new([
            (
                FieldPath::parse("avatar").unwrap(),
                Value::Binary(vec![3; 48 * 1024]),
            ),
            (
                FieldPath::parse("empty").unwrap(),
                Value::List(TypedList::new(ValueType::Integer, vec![]).unwrap()),
            ),
            (
                FieldPath::parse("nested").unwrap(),
                Value::List(
                    TypedList::new(
                        ValueType::List(Box::new(ValueType::String)),
                        vec![Value::List(
                            TypedList::new(ValueType::String, vec![]).unwrap(),
                        )],
                    )
                    .unwrap(),
                ),
            ),
        ])
        .unwrap(),
    };
    let bytes = encode_projection(&projection).unwrap();
    assert!(bytes.len() > MAX_VALUE_BYTES);
    assert_eq!(
        decode_projection(&bytes, &mut budget()).unwrap(),
        projection
    );
    let entry = IndexEntry {
        generation: projection.generation.clone(),
        location: location(),
        projection: Some(object(&bytes)),
    };
    let encoded = encode_index_entry(&entry).unwrap();
    assert!(encoded.len() < MAX_VALUE_BYTES);
    assert_eq!(decode_index_entry(&encoded, &mut budget()).unwrap(), entry);
    assert!(decode_index_entry(&bytes, &mut budget()).is_err());
    let mut invalid = entry;
    invalid.location.range = std::ops::Range { start: 20, end: 10 };
    assert!(encode_index_entry(&invalid).is_err());
    let mut invalid = descriptor();
    invalid.definition.len = (MAX_VALUE_BYTES + 1) as u32;
    assert!(encode_index_descriptor(&invalid).is_err());
}

#[test]
fn tagged_outcomes_preserve_document_receipts_and_management_results() {
    let mut publication = receipt(IndexOperation::Create, IndexState::Building).publication;
    // Original document receipts can exceed both page and descriptor limits.
    publication.revisions = (0..256)
        .map(|i| {
            (
                DocumentPath::from_segments([format!("{i:04}{}", "x".repeat(1000))]).unwrap(),
                Some(Revision::new("management-commit").unwrap()),
            )
        })
        .collect();
    let record = format::RequestRecord {
        fingerprint: [7; 32],
        receipt: publication,
    };
    let old = encode_outcome(&record).unwrap();
    assert_eq!(decode_outcome(&old).unwrap(), record);
    let original = TaggedOutcome::Document(record.clone());
    let bytes = encode_tagged_outcome(&original).unwrap();
    assert!(bytes.len() > MAX_PAGE_BYTES);
    assert_eq!(
        decode_tagged_outcome(&bytes, &mut budget()).unwrap(),
        original
    );
    assert!(matches!(
        decode_outcome(&bytes),
        Err(Error::UnsupportedVersion(2))
    ));
    for (operation, state) in [
        (IndexOperation::Create, IndexState::Building),
        (IndexOperation::Rebuild, IndexState::Building),
        (IndexOperation::Advance, IndexState::Building),
        (IndexOperation::Advance, IndexState::Active),
        (IndexOperation::Advance, IndexState::Failed),
        (IndexOperation::Drop, IndexState::Dropped),
    ] {
        let mut receipt = receipt(operation, state);
        if state == IndexState::Failed {
            receipt.failure = Some(IndexBuildFailure::ScalarKeyTooLarge(path("people/a")));
        }
        let original = TaggedOutcome::Index {
            fingerprint: [9; 32],
            receipt,
        };
        let bytes = encode_tagged_outcome(&original).unwrap();
        assert_eq!(
            decode_tagged_outcome(&bytes, &mut budget()).unwrap(),
            original
        );
    }
    let mut invalid = receipt(IndexOperation::Create, IndexState::Active);
    assert!(
        encode_tagged_outcome(&TaggedOutcome::Index {
            fingerprint: [0; 32],
            receipt: invalid.clone()
        })
        .is_err()
    );
    invalid.operation = IndexOperation::Advance;
    invalid.state = IndexState::Failed;
    assert!(
        encode_tagged_outcome(&TaggedOutcome::Index {
            fingerprint: [0; 32],
            receipt: invalid
        })
        .is_err()
    );
}

#[test]
fn management_fingerprints_bind_operation_generation_definition_and_limits() {
    let handle = IndexHandle::new(definition().id().clone(), generation("generation"));
    let options = IndexBuildOptions::default();
    let commands = [
        IndexCommand::Create(definition()),
        IndexCommand::Rebuild(handle.clone()),
        IndexCommand::Drop(handle.clone()),
        IndexCommand::Advance(handle.clone(), options),
        IndexCommand::Advance(handle.clone(), IndexBuildOptions::new(1).unwrap()),
        IndexCommand::Advance(handle.clone(), options.with_source_byte_limit(1).unwrap()),
        IndexCommand::Advance(
            IndexHandle::new(handle.id().clone(), generation("other")),
            options,
        ),
    ];
    let hashes: std::collections::BTreeSet<_> = commands
        .iter()
        .map(|command| index_command_fingerprint(command).unwrap())
        .collect();
    assert_eq!(hashes.len(), commands.len());
    assert_eq!(
        index_command_fingerprint(&commands[0]).unwrap(),
        index_command_fingerprint(&IndexCommand::Create(definition())).unwrap()
    );
    let tx = Transaction::new(
        RequestId::new("request").unwrap(),
        vec![Mutation {
            path: path("people/a"),
            condition: Condition::Any,
            kind: MutationKind::Delete,
        }],
        vec![],
    )
    .unwrap();
    assert!(!hashes.contains(&format::transaction_fingerprint(&tx).unwrap()));
}

#[test]
fn valid_checksums_do_not_bypass_semantic_and_budget_bounds() {
    let descriptor = encode_index_descriptor(&descriptor()).unwrap();
    assert!(matches!(
        decode_index_descriptor(&descriptor, &mut 0),
        Err(Error::ResourceLimit(_))
    ));
    let head = encode_head_v3(&head()).unwrap();
    assert!(matches!(
        decode_head_v3(&head, &mut 1),
        Err(Error::ResourceLimit(_))
    ));
    let root = encode_root_v3(&root()).unwrap();
    assert!(matches!(
        decode_root_v3(&root, &mut 1),
        Err(Error::ResourceLimit(_))
    ));
    let mut remaining = budget();
    decode_root_v3(&root, &mut remaining).unwrap();
    let used = budget() - remaining;
    assert!(used > 0);
    let mut shared = used + 1;
    decode_root_v3(&root, &mut shared).unwrap();
    assert!(matches!(
        decode_root_v3(&root, &mut shared),
        Err(Error::ResourceLimit(_))
    ));
    let mut payload = format::payload(&descriptor, DESCRIPTOR_KIND)
        .unwrap()
        .to_vec();
    // Hostile identity length is rejected before allocation, with a valid checksum.
    payload[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(
        decode_index_descriptor(
            &format::envelope(DESCRIPTOR_KIND, payload).unwrap(),
            &mut budget()
        )
        .is_err()
    );
    let mut future = descriptor.clone();
    future[5..7].copy_from_slice(&2_u16.to_le_bytes());
    assert!(matches!(
        decode_index_descriptor(&future, &mut budget()),
        Err(Error::UnsupportedVersion(2))
    ));
    for i in [0, descriptor.len() / 2, descriptor.len() - 1] {
        let mut bad = descriptor.clone();
        bad[i] ^= 1;
        assert!(decode_index_descriptor(&bad, &mut budget()).is_err());
    }
    let mut trailing = descriptor;
    trailing.push(0);
    assert!(decode_index_descriptor(&trailing, &mut budget()).is_err());
    let root = format::envelope_version(2, 3, vec![0; MAX_ROOT_BYTES]).unwrap();
    assert!(decode_root_v3(&root, &mut budget()).is_err());
}

proptest! {
    #[test]
    fn arbitrary_and_checksummed_payloads_stay_bounded(payload in prop::collection::vec(any::<u8>(), 0..4096)) {
        let _ = decode_root_v3(&payload, &mut 4096);
        let _ = decode_head_v3(&payload, &mut 4096);
        let _ = decode_index_descriptor(&payload, &mut 4096);
        let _ = decode_index_entry(&payload, &mut 4096);
        let _ = decode_projection(&payload, &mut 4096);
        let _ = decode_tagged_outcome(&payload, &mut 4096);
        let _ = decode_root_v3(&format::envelope_version(2, 3, payload.clone()).unwrap(), &mut 4096);
        let _ = decode_head_v3(&format::envelope_version(1, 3, payload.clone()).unwrap(), &mut 4096);
        let _ = decode_index_descriptor(&format::envelope(DESCRIPTOR_KIND, payload.clone()).unwrap(), &mut 4096);
        let _ = decode_index_entry(&format::envelope(INDEX_ENTRY_KIND, payload.clone()).unwrap(), &mut 4096);
        let _ = decode_projection(&format::envelope(PROJECTION_KIND, payload.clone()).unwrap(), &mut 4096);
        let _ = decode_tagged_outcome(&format::envelope_version(OUTCOME_KIND, 2, payload).unwrap(), &mut 4096);
    }
}
