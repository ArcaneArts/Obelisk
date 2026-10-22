use super::codec::Node;
use super::*;
use crate::{Error, format};
use obelisk_types::*;
use proptest::prelude::*;

fn definition(scalar_type: ScalarType, order: IndexOrder) -> IndexDefinition {
    IndexDefinition::new(
        IndexId::new("test-index").unwrap(),
        IndexScope::Descendants(QueryScope::Root),
        vec![IndexField::new(
            FieldPath::parse("value").unwrap(),
            scalar_type,
            order,
        )],
        vec![],
    )
    .unwrap()
}
fn path(index: usize) -> DocumentPath {
    DocumentPath::parse(&format!("documents/{index:04}")).unwrap()
}
fn assert_order(mut values: Vec<Value>, scalar_type: ScalarType, order: IndexOrder) {
    let definition = definition(scalar_type, order);
    let compare = |left: &Value, right: &Value| match (left, right) {
        (Value::Boolean(left), Value::Boolean(right)) => left.cmp(right),
        (Value::Integer(left), Value::Integer(right)) => left.cmp(right),
        (Value::Double(left), Value::Double(right)) => {
            left.get().partial_cmp(&right.get()).unwrap()
        }
        (Value::String(left), Value::String(right)) => left.cmp(right),
        _ => unreachable!(),
    };
    values.sort_by(compare);
    if order == IndexOrder::Descending {
        values.reverse();
    }
    let mut encoded: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(position, value)| {
            (
                index_entry_key(&definition, std::slice::from_ref(value), &path(position)).unwrap(),
                value.clone(),
            )
        })
        .collect();
    encoded.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        encoded
            .into_iter()
            .map(|(_, value)| value)
            .collect::<Vec<_>>(),
        values
    );
}

#[test]
fn all_scalar_encodings_follow_declared_order() {
    for order in [IndexOrder::Ascending, IndexOrder::Descending] {
        assert_order(
            vec![Value::Boolean(false), Value::Boolean(true)],
            ScalarType::Boolean,
            order,
        );
        assert_order(
            vec![
                Value::Integer(i64::MAX),
                Value::Integer(-1),
                Value::Integer(0),
                Value::Integer(i64::MIN),
                Value::Integer(1),
            ],
            ScalarType::Integer,
            order,
        );
        assert_order(
            [-f64::MAX, -10.5, -0.5, -0.0, 0.5, 10.5, f64::MAX]
                .into_iter()
                .map(|value| Value::double(value).unwrap())
                .collect(),
            ScalarType::Double,
            order,
        );
        assert_order(
            ["", "\0", "\0a", "a", "a\0", "aa", "é", "🙂"]
                .into_iter()
                .map(|value| Value::String(value.into()))
                .collect(),
            ScalarType::String,
            order,
        );
    }
}

#[test]
fn composite_keys_roundtrip_prefix_and_tie_break_by_structured_path() {
    let definition = IndexDefinition::new(
        IndexId::new("composite").unwrap(),
        IndexScope::Descendants(QueryScope::Root),
        vec![
            IndexField::new(
                FieldPath::parse("name").unwrap(),
                ScalarType::String,
                IndexOrder::Ascending,
            ),
            IndexField::new(
                FieldPath::parse("score").unwrap(),
                ScalarType::Integer,
                IndexOrder::Descending,
            ),
            IndexField::new(
                FieldPath::parse("enabled").unwrap(),
                ScalarType::Boolean,
                IndexOrder::Ascending,
            ),
        ],
        vec![FieldPath::parse("email").unwrap()],
    )
    .unwrap();
    let values = vec![
        Value::String("a\0é".into()),
        Value::Integer(-42),
        Value::Boolean(true),
    ];
    let paths = [
        DocumentPath::parse("people/a").unwrap(),
        DocumentPath::parse("people/a/b").unwrap(),
        DocumentPath::from_segments(["people", "a/b"]).unwrap(),
    ];
    let prefix = index_value_prefix(&definition, &values).unwrap();
    let end = index_prefix_end(&prefix).unwrap();
    let mut keys = paths
        .iter()
        .map(|path| index_entry_key(&definition, &values, path).unwrap())
        .collect::<Vec<_>>();
    for (key, path) in keys.iter().zip(paths.iter()) {
        assert!(key.starts_with(&prefix));
        assert!(key < &end);
        assert_eq!(
            decode_index_entry_key(&definition, key).unwrap(),
            (values.clone(), path.clone())
        );
    }
    keys.sort();
    let mut expected = paths.to_vec();
    expected.sort();
    assert_eq!(
        keys.iter()
            .map(|key| decode_index_entry_key(&definition, key).unwrap().1)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(matches!(
        index_entry_key(&definition, &values[..2], &paths[0]),
        Err(Error::Validation(_))
    ));
}

#[test]
fn definition_codec_and_document_projection_are_strict() {
    let definition = IndexDefinition::new(
        IndexId::new("by-score").unwrap(),
        IndexScope::Children(QueryScope::path("people").unwrap()),
        vec![IndexField::new(
            FieldPath::parse("score").unwrap(),
            ScalarType::Integer,
            IndexOrder::Descending,
        )],
        vec![FieldPath::parse("name").unwrap()],
    )
    .unwrap();
    let bytes = encode_index_definition(&definition).unwrap();
    assert_eq!(decode_index_definition(&bytes).unwrap(), definition);
    assert_eq!(
        index_definition_id(&index_definition_key(definition.id())).unwrap(),
        *definition.id()
    );
    let document = Document::new([
        (FieldPath::parse("score").unwrap(), Value::Integer(7)),
        (
            FieldPath::parse("name").unwrap(),
            Value::String("Ada".into()),
        ),
    ])
    .unwrap();
    assert!(
        index_entry_for_document(
            &definition,
            &DocumentPath::parse("people/ada").unwrap(),
            &document,
        )
        .unwrap()
        .is_some()
    );
    assert!(
        index_entry_for_document(
            &definition,
            &DocumentPath::parse("people/ada/deep").unwrap(),
            &document,
        )
        .unwrap()
        .is_none()
    );
    assert!(matches!(
        index_entry_key(
            &definition,
            &[Value::Integer(7)],
            &DocumentPath::parse("outside/ada").unwrap(),
        ),
        Err(Error::Validation(_))
    ));
    let wrong = Document::new([(
        FieldPath::parse("score").unwrap(),
        Value::double(7.0).unwrap(),
    )])
    .unwrap();
    assert!(
        index_entry_for_document(
            &definition,
            &DocumentPath::parse("people/ada").unwrap(),
            &wrong,
        )
        .unwrap()
        .is_none()
    );
    let mut corrupt = bytes;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    assert!(matches!(
        decode_index_definition(&corrupt),
        Err(Error::Corrupt(_))
    ));
    let mut future = encode_index_definition(&definition).unwrap();
    future[5..7].copy_from_slice(&2_u16.to_le_bytes());
    assert!(matches!(
        decode_index_definition(&future),
        Err(Error::UnsupportedVersion(2))
    ));
    let ordered = IndexDefinition::new(
        IndexId::new("covered-order").unwrap(),
        IndexScope::Descendants(QueryScope::Root),
        vec![IndexField::new(
            FieldPath::parse("key").unwrap(),
            ScalarType::Integer,
            IndexOrder::Ascending,
        )],
        vec![
            FieldPath::parse("covered-a").unwrap(),
            FieldPath::parse("covered-z").unwrap(),
        ],
    )
    .unwrap();
    let mut noncanonical = encode_index_definition(&ordered).unwrap();
    let position = |bytes: &[u8], needle: &[u8]| {
        bytes
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap()
    };
    let a = format::HEADER_BYTES + position(&noncanonical[format::HEADER_BYTES..], b"covered-a");
    let z = format::HEADER_BYTES + position(&noncanonical[format::HEADER_BYTES..], b"covered-z");
    noncanonical[a..a + 9].copy_from_slice(b"covered-z");
    noncanonical[z..z + 9].copy_from_slice(b"covered-a");
    let hash = *blake3::hash(&noncanonical[format::HEADER_BYTES..]).as_bytes();
    noncanonical[11..format::HEADER_BYTES].copy_from_slice(&hash);
    assert!(matches!(
        decode_index_definition(&noncanonical),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn scalar_key_bounds_and_malformed_keys_fail_closed() {
    let strings = definition(ScalarType::String, IndexOrder::Ascending);
    assert!(matches!(
        index_entry_key(
            &strings,
            &[Value::String("\0".repeat(MAX_INDEX_SCALAR_BYTES))],
            &DocumentPath::parse("x").unwrap(),
        ),
        Err(Error::ResourceLimit(_))
    ));
    for malformed in [
        vec![],
        vec![255],
        vec![4, 0],
        vec![4, 0, 1, b'x', 0],
        vec![4, 255, 255],
    ] {
        assert!(decode_index_entry_key(&strings, &malformed).is_err());
    }
    let doubles = definition(ScalarType::Double, IndexOrder::Ascending);
    let mut negative_zero = vec![3];
    negative_zero.extend_from_slice(&0x7fff_ffff_ffff_ffff_u64.to_be_bytes());
    negative_zero.extend_from_slice(&document_key(&DocumentPath::parse("x").unwrap()));
    assert!(matches!(
        decode_index_entry_key(&doubles, &negative_zero),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn index_page_families_are_distinct() {
    let definition_page = Node {
        level: 0,
        entries: vec![(b"definition".to_vec(), vec![])],
    }
    .encode(Family::IndexDefinitions)
    .unwrap();
    assert!(Node::decode(&definition_page, Family::IndexDefinitions).is_ok());
    assert!(matches!(
        Node::decode(&definition_page, Family::ScalarIndexes),
        Err(Error::Corrupt(_))
    ));
    let entry_page = Node {
        level: 0,
        entries: vec![(b"entry".to_vec(), vec![])],
    }
    .encode(Family::ScalarIndexes)
    .unwrap();
    assert!(Node::decode(&entry_page, Family::ScalarIndexes).is_ok());
}

proptest! {
    #[test]
    fn integer_key_order_matches_values(left in any::<i64>(), right in any::<i64>()) {
        let definition = definition(ScalarType::Integer, IndexOrder::Ascending);
        let path = DocumentPath::parse("x").unwrap();
        let left_key = index_entry_key(&definition, &[Value::Integer(left)], &path).unwrap();
        let right_key = index_entry_key(&definition, &[Value::Integer(right)], &path).unwrap();
        prop_assert_eq!(left.cmp(&right), left_key.cmp(&right_key));
    }

    #[test]
    fn finite_double_key_order_matches_values(left in any::<f64>().prop_filter("finite", |v| v.is_finite()), right in any::<f64>().prop_filter("finite", |v| v.is_finite())) {
        let left = if left == 0.0 { 0.0 } else { left };
        let right = if right == 0.0 { 0.0 } else { right };
        let definition = definition(ScalarType::Double, IndexOrder::Ascending);
        let path = DocumentPath::parse("x").unwrap();
        let left_key = index_entry_key(&definition, &[Value::double(left).unwrap()], &path).unwrap();
        let right_key = index_entry_key(&definition, &[Value::double(right).unwrap()], &path).unwrap();
        prop_assert_eq!(left.partial_cmp(&right).unwrap(), left_key.cmp(&right_key));
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..10_000)) {
        let definition = definition(ScalarType::String, IndexOrder::Descending);
        let _ = decode_index_definition(&bytes);
        let _ = decode_index_entry_key(&definition, &bytes);
        let _ = index_definition_id(&bytes);
    }
}
