use crate::{DocumentPath, FieldPath, QueryScope, Result, Value, invalid};
use std::{collections::BTreeSet, fmt};

pub const MAX_INDEX_FIELDS: usize = 8;
pub const MAX_COVERED_FIELDS: usize = 32;
/// Conservative public bound leaving room for the engine's versioned envelope.
pub const MAX_INDEX_DEFINITION_BYTES: usize = 7 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexId(String);
impl IndexId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(invalid("invalid index identity"));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for IndexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarType {
    String,
    Integer,
    Double,
    Boolean,
}
impl ScalarType {
    pub fn matches(self, value: &Value) -> bool {
        matches!(
            (self, value),
            (Self::String, Value::String(_))
                | (Self::Integer, Value::Integer(_))
                | (Self::Double, Value::Double(_))
                | (Self::Boolean, Value::Boolean(_))
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexOrder {
    Ascending,
    Descending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexField {
    path: FieldPath,
    scalar_type: ScalarType,
    order: IndexOrder,
}
impl IndexField {
    pub fn new(path: FieldPath, scalar_type: ScalarType, order: IndexOrder) -> Self {
        Self {
            path,
            scalar_type,
            order,
        }
    }
    pub fn path(&self) -> &FieldPath {
        &self.path
    }
    pub fn scalar_type(&self) -> ScalarType {
        self.scalar_type
    }
    pub fn order(&self) -> IndexOrder {
        self.order
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexScope {
    Children(QueryScope),
    Descendants(QueryScope),
}
impl IndexScope {
    pub fn contains(&self, path: &DocumentPath) -> bool {
        match self {
            Self::Children(QueryScope::Root) => path.segments().len() == 1,
            Self::Descendants(QueryScope::Root) => true,
            Self::Children(QueryScope::Path(parent)) => {
                parent.is_ancestor_of(path) && path.segments().len() == parent.segments().len() + 1
            }
            Self::Descendants(QueryScope::Path(parent)) => parent.is_ancestor_of(path),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDefinition {
    id: IndexId,
    scope: IndexScope,
    fields: Vec<IndexField>,
    covered_fields: Vec<FieldPath>,
}
impl IndexDefinition {
    pub fn new(
        id: IndexId,
        scope: IndexScope,
        fields: Vec<IndexField>,
        mut covered_fields: Vec<FieldPath>,
    ) -> Result<Self> {
        if fields.is_empty() || fields.len() > MAX_INDEX_FIELDS {
            return Err(invalid("index must have 1..=8 ordered fields"));
        }
        if covered_fields.len() > MAX_COVERED_FIELDS {
            return Err(invalid("too many covered fields"));
        }
        let mut indexed = BTreeSet::new();
        for field in &fields {
            if !indexed.insert(field.path.clone()) {
                return Err(invalid("duplicate indexed field"));
            }
        }
        for left in 0..fields.len() {
            for right in left + 1..fields.len() {
                if fields[left].path.is_ancestor_of(&fields[right].path)
                    || fields[right].path.is_ancestor_of(&fields[left].path)
                {
                    return Err(invalid("conflicting indexed fields"));
                }
            }
        }
        covered_fields.sort();
        if covered_fields.windows(2).any(|pair| pair[0] == pair[1])
            || covered_fields.iter().any(|path| indexed.contains(path))
        {
            return Err(invalid("duplicate indexed or covered field"));
        }
        let definition = Self {
            id,
            scope,
            fields,
            covered_fields,
        };
        if definition.estimated_size() > MAX_INDEX_DEFINITION_BYTES {
            return Err(invalid("index definition byte limit"));
        }
        Ok(definition)
    }
    pub fn id(&self) -> &IndexId {
        &self.id
    }
    pub fn scope(&self) -> &IndexScope {
        &self.scope
    }
    pub fn fields(&self) -> &[IndexField] {
        &self.fields
    }
    pub fn covered_fields(&self) -> &[FieldPath] {
        &self.covered_fields
    }
    pub fn estimated_size(&self) -> usize {
        fn path(path: &FieldPath) -> usize {
            4 + path
                .segments()
                .iter()
                .map(|segment| 4 + segment.len())
                .sum::<usize>()
        }
        fn scope(scope: &IndexScope) -> usize {
            let query = match scope {
                IndexScope::Children(query) | IndexScope::Descendants(query) => query,
            };
            2 + match query {
                QueryScope::Root => 0,
                QueryScope::Path(path) => {
                    4 + path
                        .segments()
                        .iter()
                        .map(|segment| 4 + segment.len())
                        .sum::<usize>()
                }
            }
        }
        32 + self.id.as_str().len()
            + scope(&self.scope)
            + self
                .fields
                .iter()
                .map(|field| 2 + path(&field.path))
                .sum::<usize>()
            + self.covered_fields.iter().map(path).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(path: &str) -> IndexField {
        IndexField::new(
            FieldPath::parse(path).unwrap(),
            ScalarType::String,
            IndexOrder::Ascending,
        )
    }

    #[test]
    fn definitions_are_bounded_canonical_and_unambiguous() {
        let definition = IndexDefinition::new(
            IndexId::new("by-name").unwrap(),
            IndexScope::Descendants(QueryScope::path("people").unwrap()),
            vec![field("name"), field("address.city")],
            vec![
                FieldPath::parse("profile.email").unwrap(),
                FieldPath::parse("profile.avatar").unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(definition.covered_fields()[0].to_string(), "profile.avatar");
        assert!(
            definition
                .scope()
                .contains(&DocumentPath::parse("people/a").unwrap())
        );
        assert!(
            !definition
                .scope()
                .contains(&DocumentPath::parse("other/a").unwrap())
        );
        assert!(IndexId::new("").is_err());
        assert!(
            IndexDefinition::new(
                IndexId::new("empty").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                vec![],
                vec![],
            )
            .is_err()
        );
        assert!(
            IndexDefinition::new(
                IndexId::new("duplicate").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                vec![field("name"), field("name")],
                vec![],
            )
            .is_err()
        );
        assert!(
            IndexDefinition::new(
                IndexId::new("conflict").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                vec![field("name"), field("name.first")],
                vec![],
            )
            .is_err()
        );
        assert!(
            IndexDefinition::new(
                IndexId::new("too-many-fields").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                (0..=MAX_INDEX_FIELDS)
                    .map(|index| field(&format!("field{index}")))
                    .collect(),
                vec![],
            )
            .is_err()
        );
        assert!(
            IndexDefinition::new(
                IndexId::new("too-many-covered").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                vec![field("key")],
                (0..=MAX_COVERED_FIELDS)
                    .map(|index| FieldPath::parse(&format!("field{index}")).unwrap())
                    .collect(),
            )
            .is_err()
        );
        assert!(
            IndexDefinition::new(
                IndexId::new("too-large").unwrap(),
                IndexScope::Descendants(QueryScope::Root),
                vec![
                    field(&format!("a{}", "x".repeat(3600))),
                    field(&format!("b{}", "x".repeat(3600))),
                ],
                vec![],
            )
            .is_err()
        );
    }

    #[test]
    fn scalar_types_are_exact() {
        assert!(ScalarType::Integer.matches(&Value::Integer(1)));
        assert!(!ScalarType::Integer.matches(&Value::double(1.0).unwrap()));
        assert!(!ScalarType::String.matches(&Value::Binary(vec![])));
    }
}
