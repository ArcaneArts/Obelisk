//! Validated indexed-query inputs, independent of keys and persistent encodings.
use crate::*;
use std::ops::Bound;

pub const MAX_QUERY_FIELDS: usize = MAX_FIELDS;
pub const MAX_QUERY_SPEC_BYTES: usize = 64 * 1024;

fn scalar(value: &Value) -> Result<ScalarType> {
    value.validate()?;
    match value {
        Value::String(_) => Ok(ScalarType::String),
        Value::Integer(_) => Ok(ScalarType::Integer),
        Value::Double(_) => Ok(ScalarType::Double),
        Value::Boolean(_) => Ok(ScalarType::Boolean),
        _ => Err(invalid("indexed predicates require scalar values")),
    }
}
fn field_bytes(path: &FieldPath) -> usize {
    4 + path.segments().iter().map(|s| s.len() + 4).sum::<usize>()
}

#[derive(Debug, Clone, PartialEq)]
pub struct EqualityPredicate {
    field: FieldPath,
    value: Value,
    scalar_type: ScalarType,
}
impl EqualityPredicate {
    pub fn new(field: FieldPath, value: Value) -> Result<Self> {
        let scalar_type = scalar(&value)?;
        if field_bytes(&field) + value.estimated_size()? > MAX_QUERY_SPEC_BYTES {
            return Err(invalid("query predicate byte limit"));
        }
        Ok(Self {
            field,
            value,
            scalar_type,
        })
    }
    pub fn field(&self) -> &FieldPath {
        &self.field
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn scalar_type(&self) -> ScalarType {
        self.scalar_type
    }
}

/// Bounds apply to the first ordered component, after the equality prefix.
#[derive(Debug, Clone, PartialEq)]
pub struct ScalarRange {
    lower: Bound<Value>,
    upper: Bound<Value>,
    scalar_type: ScalarType,
}
impl ScalarRange {
    pub fn new(lower: Bound<Value>, upper: Bound<Value>) -> Result<Self> {
        let values: Vec<_> = [&lower, &upper]
            .into_iter()
            .filter_map(|b| match b {
                Bound::Included(v) | Bound::Excluded(v) => Some(v),
                Bound::Unbounded => None,
            })
            .collect();
        let first = values
            .first()
            .ok_or_else(|| invalid("range needs a bound"))?;
        let scalar_type = scalar(first)?;
        if values.iter().any(|v| !scalar_type.matches(v)) {
            return Err(invalid("range bounds require the same scalar type"));
        }
        if values
            .iter()
            .map(|v| v.estimated_size())
            .sum::<Result<usize>>()?
            > MAX_QUERY_SPEC_BYTES
        {
            return Err(invalid("query range byte limit"));
        }
        Ok(Self {
            lower,
            upper,
            scalar_type,
        })
    }
    fn estimated_size(&self) -> Result<usize> {
        [&self.lower, &self.upper]
            .into_iter()
            .filter_map(|bound| match bound {
                Bound::Included(value) | Bound::Excluded(value) => Some(value),
                Bound::Unbounded => None,
            })
            .map(Value::estimated_size)
            .sum()
    }
    pub fn lower(&self) -> &Bound<Value> {
        &self.lower
    }
    pub fn upper(&self) -> &Bound<Value> {
        &self.upper
    }
    pub fn scalar_type(&self) -> ScalarType {
        self.scalar_type
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projection {
    Full,
    Fields(Vec<FieldPath>),
}
impl Projection {
    pub fn fields(mut fields: Vec<FieldPath>) -> Result<Self> {
        if fields.len() > MAX_QUERY_FIELDS {
            return Err(invalid("projection field limit"));
        }
        fields.sort();
        if fields.windows(2).any(|p| p[0] == p[1]) {
            return Err(invalid("duplicate projection field"));
        }
        if fields.iter().map(field_bytes).sum::<usize>() > MAX_QUERY_SPEC_BYTES {
            return Err(invalid("projection byte limit"));
        }
        Ok(Self::Fields(fields))
    }
}

/// A useful missing-index signature. Equality directions are immaterial; all
/// remaining component directions and the exact hierarchy scope must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRequirement {
    pub scope: IndexScope,
    pub equality: Vec<(FieldPath, ScalarType)>,
    pub order: Vec<IndexField>,
}
impl IndexRequirement {
    pub fn matches(&self, definition: &IndexDefinition) -> bool {
        self.scope == *definition.scope()
            && definition.fields().len() == self.equality.len() + self.order.len()
            && self
                .equality
                .iter()
                .zip(definition.fields())
                .all(|((path, kind), field)| path == field.path() && *kind == field.scalar_type())
            && definition.fields()[self.equality.len()..] == self.order
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexedQuery {
    requirement: IndexRequirement,
    equality: Vec<EqualityPredicate>,
    range: Option<ScalarRange>,
    projection: Projection,
    index: Option<IndexId>,
    estimated_size: usize,
}
impl IndexedQuery {
    pub fn new(
        scope: IndexScope,
        equality: Vec<EqualityPredicate>,
        range: Option<ScalarRange>,
        order: Vec<IndexField>,
        projection: Projection,
    ) -> Result<Self> {
        // Revalidate public enum variants too; callers can construct Fields directly.
        let projection = match projection {
            Projection::Full => Projection::Full,
            Projection::Fields(fields) => Projection::fields(fields)?,
        };
        if equality.len() + order.len() > MAX_INDEX_FIELDS {
            return Err(invalid("query key field limit"));
        }
        let fields = equality
            .iter()
            .map(|p| IndexField::new(p.field.clone(), p.scalar_type, IndexOrder::Ascending))
            .chain(order.iter().cloned())
            .collect();
        let shape =
            IndexDefinition::new(IndexId::new("query-shape")?, scope.clone(), fields, vec![])?;
        if let Some(range) = &range
            && order
                .first()
                .is_none_or(|f| f.scalar_type() != range.scalar_type)
        {
            return Err(invalid("range requires a matching first ordered component"));
        }
        let estimated_size = shape.estimated_size()
            + equality
                .iter()
                .map(|p| p.value.estimated_size())
                .sum::<Result<usize>>()?
            + range.as_ref().map_or(Ok(0), ScalarRange::estimated_size)?
            + match &projection {
                Projection::Full => 1,
                Projection::Fields(fields) => fields.iter().map(field_bytes).sum(),
            };
        if estimated_size > MAX_QUERY_SPEC_BYTES {
            return Err(invalid("query specification byte limit"));
        }
        let requirement = IndexRequirement {
            scope,
            equality: equality
                .iter()
                .map(|p| (p.field.clone(), p.scalar_type))
                .collect(),
            order,
        };
        Ok(Self {
            requirement,
            equality,
            range,
            projection,
            index: None,
            estimated_size,
        })
    }
    pub fn with_index(mut self, index: IndexId) -> Self {
        self.index = Some(index);
        self
    }
    pub fn requirement(&self) -> &IndexRequirement {
        &self.requirement
    }
    pub fn equality(&self) -> &[EqualityPredicate] {
        &self.equality
    }
    pub fn range(&self) -> Option<&ScalarRange> {
        self.range.as_ref()
    }
    pub fn projection(&self) -> &Projection {
        &self.projection
    }
    pub fn index(&self) -> Option<&IndexId> {
        self.index.as_ref()
    }
    pub fn estimated_size(&self) -> usize {
        self.estimated_size + self.index.as_ref().map_or(0, |id| id.as_str().len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn field(name: &str, kind: ScalarType) -> IndexField {
        IndexField::new(FieldPath::parse(name).unwrap(), kind, IndexOrder::Ascending)
    }
    #[test]
    fn indexed_shapes_types_projections_and_range_position_are_validated() {
        let path = FieldPath::parse("score").unwrap();
        assert!(EqualityPredicate::new(path.clone(), Value::Binary(vec![])).is_err());
        assert!(ScalarRange::new(Bound::Unbounded, Bound::Unbounded).is_err());
        assert!(
            ScalarRange::new(
                Bound::Included(Value::Integer(1)),
                Bound::Included(Value::double(1.0).unwrap())
            )
            .is_err()
        );
        let scope = IndexScope::Descendants(QueryScope::Root);
        let range = ScalarRange::new(Bound::Included(Value::Integer(1)), Bound::Unbounded).unwrap();
        assert!(
            IndexedQuery::new(
                scope.clone(),
                vec![],
                Some(range.clone()),
                vec![],
                Projection::Full
            )
            .is_err()
        );
        assert!(
            IndexedQuery::new(
                scope.clone(),
                vec![],
                Some(range),
                vec![field("score", ScalarType::Double)],
                Projection::Full
            )
            .is_err()
        );
        assert!(
            IndexedQuery::new(
                scope.clone(),
                vec![],
                None,
                vec![field("score", ScalarType::Integer)],
                Projection::Fields(vec![path.clone(), path.clone()])
            )
            .is_err()
        );
        assert!(
            IndexedQuery::new(
                scope.clone(),
                vec![EqualityPredicate::new(path.clone(), Value::Integer(1)).unwrap()],
                None,
                vec![field("score", ScalarType::Integer)],
                Projection::Full
            )
            .is_err()
        );
        let query = IndexedQuery::new(
            scope.clone(),
            vec![],
            None,
            vec![field("score", ScalarType::Integer)],
            Projection::fields(vec![FieldPath::parse("z").unwrap(), path.clone()]).unwrap(),
        )
        .unwrap();
        let reversed = IndexedQuery::new(
            scope.clone(),
            vec![],
            None,
            vec![field("score", ScalarType::Integer)],
            Projection::Fields(vec![path, FieldPath::parse("z").unwrap()]),
        )
        .unwrap();
        assert_eq!(query, reversed);
        let extra = IndexDefinition::new(
            IndexId::new("extra").unwrap(),
            scope,
            vec![
                field("score", ScalarType::Integer),
                field("missing", ScalarType::String),
            ],
            vec![],
        )
        .unwrap();
        assert!(!query.requirement().matches(&extra));
        assert!(
            Projection::fields(
                (0..=MAX_QUERY_FIELDS)
                    .map(|n| FieldPath::parse(&format!("f{n}")).unwrap())
                    .collect()
            )
            .is_err()
        );
        assert!(
            EqualityPredicate::new(
                FieldPath::parse("x").unwrap(),
                Value::String("x".repeat(MAX_QUERY_SPEC_BYTES))
            )
            .is_err()
        );
    }
}
