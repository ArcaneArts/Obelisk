//! Exact-shape planning and composite range construction. Never plans a scan.
use crate::{
    Error, Result, Snapshot, format,
    index_catalog::LoadedIndex,
    metadata::{self, Budget},
};
use obelisk_types::*;
use std::ops::Bound;

pub(crate) fn identity(query: &IndexedQuery, budget: &mut Budget) -> Result<[u8; 32]> {
    // Covers input clones, canonical encoding, bounds and their small vectors.
    budget.charge_decoded(16 * query.estimated_size() + 8 * metadata::MAX_KEY_BYTES)?;
    let requirement = query.requirement();
    let fields = requirement
        .equality
        .iter()
        .map(|(path, kind)| IndexField::new(path.clone(), *kind, IndexOrder::Ascending))
        .chain(requirement.order.iter().cloned())
        .collect();
    let shape = IndexDefinition::new(
        IndexId::new("query-shape")?,
        requirement.scope.clone(),
        fields,
        vec![],
    )?;
    let mut writer = format::Writer::default();
    writer.bytes(b"ObeliskIndexedQuery-v1\0");
    writer.bytes(&metadata::encode_index_definition(&shape)?);
    writer.u32(query.equality().len());
    for equality in query.equality() {
        writer.value(equality.value());
    }
    if let Some(range) = query.range() {
        writer.byte(1);
        for bound in [range.lower(), range.upper()] {
            match bound {
                Bound::Unbounded => writer.byte(0),
                Bound::Included(value) => {
                    writer.byte(1);
                    writer.value(value);
                }
                Bound::Excluded(value) => {
                    writer.byte(2);
                    writer.value(value);
                }
            }
        }
    } else {
        writer.byte(0);
    }
    match query.projection() {
        Projection::Full => writer.byte(0),
        Projection::Fields(fields) => {
            writer.byte(1);
            writer.u32(fields.len());
            for field in fields {
                writer.path(field.segments());
            }
        }
    }
    if let Some(index) = query.index() {
        writer.byte(1);
        writer.text(index.as_str());
    } else {
        writer.byte(0);
    }
    Ok(*blake3::hash(&writer.0).as_bytes())
}

pub(crate) async fn choose(
    snapshot: &Snapshot,
    query: &IndexedQuery,
    budget: &mut Budget,
) -> Result<LoadedIndex> {
    let indexes = if let Some(id) = query.index() {
        snapshot.load_index(id, budget).await?.into_iter().collect()
    } else {
        snapshot.catalog(budget).await?
    };
    let mut compatible_inactive = false;
    for index in indexes {
        if query.requirement().matches(&index.definition) {
            if index.descriptor.active.is_some() {
                return Ok(index);
            }
            compatible_inactive = true;
        }
    }
    Err(Error::IndexUnavailable {
        required: Box::new(query.requirement().clone()),
        requested: query.index().cloned(),
        reason: if compatible_inactive {
            "matching index has no active generation"
        } else {
            "no index with the exact scope, types and ordering"
        },
    })
}

pub(crate) struct Bounds {
    pub start: Vec<u8>,
    pub end: Option<Vec<u8>>,
    pub empty: bool,
}
impl Bounds {
    pub fn contains(&self, key: &[u8]) -> bool {
        !self.empty
            && key >= self.start.as_slice()
            && self.end.as_ref().is_none_or(|end| key < end.as_slice())
    }
}
pub(crate) fn bounds(definition: &IndexDefinition, query: &IndexedQuery) -> Result<Bounds> {
    let values: Vec<_> = query.equality().iter().map(|p| p.value().clone()).collect();
    let prefix = metadata::index_value_prefix(definition, &values)?;
    let mut bounds = Bounds {
        end: metadata::index_prefix_end(&prefix),
        start: prefix,
        empty: false,
    };
    let Some(range) = query.range() else {
        return Ok(bounds);
    };
    let order = definition.fields()[values.len()].order();
    let (low, high) = match order {
        IndexOrder::Ascending => (range.lower(), range.upper()),
        IndexOrder::Descending => (range.upper(), range.lower()),
    };
    let encode = |value: &Value| {
        let mut values = values.clone();
        values.push(value.clone());
        metadata::index_value_prefix(definition, &values)
    };
    bounds.start = match low {
        Bound::Unbounded => bounds.start,
        Bound::Included(v) => encode(v)?,
        Bound::Excluded(v) => match metadata::index_prefix_end(&encode(v)?) {
            Some(start) => start,
            None => {
                bounds.empty = true;
                Vec::new()
            }
        },
    };
    bounds.end = match high {
        Bound::Unbounded => bounds.end,
        Bound::Included(v) => metadata::index_prefix_end(&encode(v)?),
        Bound::Excluded(v) => Some(encode(v)?),
    };
    bounds.empty |= bounds.end.as_ref().is_some_and(|end| bounds.start >= *end);
    Ok(bounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;
    fn compare(a: &Value, b: &Value) -> Ordering {
        match (a, b) {
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
            (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
            (Value::Double(a), Value::Double(b)) => a.get().total_cmp(&b.get()),
            (Value::String(a), Value::String(b)) => a.cmp(b),
            _ => panic!("unexpected scalar types"),
        }
    }
    #[test]
    fn directed_bounds_match_independent_scalar_comparisons_at_extremes() {
        let fixtures = [
            (
                ScalarType::Boolean,
                vec![Value::Boolean(false), Value::Boolean(true)],
            ),
            (
                ScalarType::Integer,
                [i64::MIN, -2, 0, 2, i64::MAX]
                    .into_iter()
                    .map(Value::Integer)
                    .collect(),
            ),
            (
                ScalarType::Double,
                [
                    -f64::MAX,
                    -f64::MIN_POSITIVE,
                    0.0,
                    f64::MIN_POSITIVE,
                    f64::MAX,
                ]
                .into_iter()
                .map(|v| Value::double(v).unwrap())
                .collect(),
            ),
            (
                ScalarType::String,
                ["", "\0", "a", "a\0x", "á", "😀"]
                    .into_iter()
                    .map(|v| Value::String(v.into()))
                    .collect(),
            ),
        ];
        for (kind, values) in fixtures {
            let choices: Vec<_> = std::iter::once(Bound::Unbounded)
                .chain(
                    values
                        .iter()
                        .flat_map(|v| [Bound::Included(v.clone()), Bound::Excluded(v.clone())]),
                )
                .collect();
            for direction in [IndexOrder::Ascending, IndexOrder::Descending] {
                for prefix in [false, true] {
                    let scalar =
                        IndexField::new(FieldPath::parse("scalar").unwrap(), kind, direction);
                    let suffix = IndexField::new(
                        FieldPath::parse("suffix").unwrap(),
                        ScalarType::Integer,
                        IndexOrder::Ascending,
                    );
                    let mut fields = vec![];
                    if prefix {
                        fields.push(IndexField::new(
                            FieldPath::parse("group").unwrap(),
                            ScalarType::Boolean,
                            IndexOrder::Descending,
                        ));
                    }
                    fields.extend([scalar.clone(), suffix.clone()]);
                    let scope = IndexScope::Descendants(QueryScope::Root);
                    let definition = IndexDefinition::new(
                        IndexId::new("test").unwrap(),
                        scope.clone(),
                        fields,
                        vec![],
                    )
                    .unwrap();
                    for lower in &choices {
                        for upper in &choices {
                            if matches!((lower, upper), (Bound::Unbounded, Bound::Unbounded)) {
                                continue;
                            }
                            let equality = if prefix {
                                vec![
                                    EqualityPredicate::new(
                                        FieldPath::parse("group").unwrap(),
                                        Value::Boolean(true),
                                    )
                                    .unwrap(),
                                ]
                            } else {
                                vec![]
                            };
                            let query = IndexedQuery::new(
                                scope.clone(),
                                equality,
                                Some(ScalarRange::new(lower.clone(), upper.clone()).unwrap()),
                                vec![scalar.clone(), suffix.clone()],
                                Projection::Full,
                            )
                            .unwrap();
                            let bounds = bounds(&definition, &query).unwrap();
                            for value in &values {
                                for suffix in [0, 1] {
                                    let mut tuple = vec![];
                                    if prefix {
                                        tuple.push(Value::Boolean(true));
                                    }
                                    tuple.extend([value.clone(), Value::Integer(suffix)]);
                                    let key = metadata::index_entry_key(
                                        &definition,
                                        &tuple,
                                        &DocumentPath::parse("people/item").unwrap(),
                                    )
                                    .unwrap();
                                    let above = match lower {
                                        Bound::Unbounded => true,
                                        Bound::Included(v) => compare(value, v) != Ordering::Less,
                                        Bound::Excluded(v) => {
                                            compare(value, v) == Ordering::Greater
                                        }
                                    };
                                    let below = match upper {
                                        Bound::Unbounded => true,
                                        Bound::Included(v) => {
                                            compare(value, v) != Ordering::Greater
                                        }
                                        Bound::Excluded(v) => compare(value, v) == Ordering::Less,
                                    };
                                    assert_eq!(
                                        bounds.contains(&key),
                                        above && below,
                                        "{kind:?} {direction:?} {lower:?}..{upper:?} value={value:?}"
                                    );
                                    if prefix {
                                        tuple[0] = Value::Boolean(false);
                                        let other = metadata::index_entry_key(
                                            &definition,
                                            &tuple,
                                            &DocumentPath::parse("people/item").unwrap(),
                                        )
                                        .unwrap();
                                        assert!(!bounds.contains(&other));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
