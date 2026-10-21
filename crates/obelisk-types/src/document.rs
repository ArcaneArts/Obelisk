use crate::{FieldPath, MAX_DOCUMENT_BYTES, MAX_FIELDS, Result, Value, invalid};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document(BTreeMap<FieldPath, Value>);
impl Document {
    pub fn new(fields: impl IntoIterator<Item = (FieldPath, Value)>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for (path, value) in fields {
            if map.insert(path, value).is_some() {
                return Err(invalid("duplicate field"));
            }
            if map.len() > MAX_FIELDS {
                return Err(invalid("field count limit"));
            }
        }
        let document = Self(map);
        document.estimated_size()?;
        let keys: Vec<_> = document.0.keys().collect();
        for pair in keys.windows(2) {
            if pair[0].is_ancestor_of(pair[1]) {
                return Err(invalid("conflicting field paths"));
            }
        }
        Ok(document)
    }
    pub fn fields(&self) -> &BTreeMap<FieldPath, Value> {
        &self.0
    }
    pub fn get(&self, field: &FieldPath) -> Option<&Value> {
        self.0.get(field)
    }
    pub fn estimated_size(&self) -> Result<usize> {
        let mut size = 4;
        for (path, value) in &self.0 {
            size += 4
                + path.segments().iter().map(|s| 4 + s.len()).sum::<usize>()
                + value.estimated_size()?;
            if size > MAX_DOCUMENT_BYTES {
                return Err(invalid("document byte limit"));
            }
        }
        Ok(size)
    }
    pub fn apply(&self, patch: &Patch) -> Result<Self> {
        let mut map = self.0.clone();
        for path in &patch.remove {
            map.remove(path);
        }
        for (path, value) in &patch.set {
            map.insert(path.clone(), value.clone());
        }
        Self::new(map)
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    set: BTreeMap<FieldPath, Value>,
    remove: Vec<FieldPath>,
}
impl Patch {
    pub fn new(
        set: impl IntoIterator<Item = (FieldPath, Value)>,
        remove: Vec<FieldPath>,
    ) -> Result<Self> {
        let set = Document::new(set)?.0;
        let mut remove = remove;
        remove.sort();
        if remove.len() > MAX_FIELDS
            || remove
                .windows(2)
                .any(|p| p[0] == p[1] || p[0].is_ancestor_of(&p[1]))
        {
            return Err(invalid(
                "duplicate, conflicting, or too many removal targets",
            ));
        }
        if remove
            .iter()
            .any(|r| set.keys().any(|s| r == s || s.is_ancestor_of(r)))
        {
            return Err(invalid("conflicting patch targets"));
        }
        Ok(Self { set, remove })
    }
    pub fn set(&self) -> &BTreeMap<FieldPath, Value> {
        &self.set
    }
    pub fn remove(&self) -> &[FieldPath] {
        &self.remove
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn p(s: &str) -> FieldPath {
        FieldPath::parse(s).unwrap()
    }
    #[test]
    fn conflicting_fields_and_patch_replacement() {
        assert!(
            Document::new([(p("a"), Value::Integer(1)), (p("a.b"), Value::Integer(2))]).is_err()
        );
        assert!(Document::new([(p("a"), Value::Integer(1)), (p("a"), Value::Integer(2))]).is_err());
        let d =
            Document::new([(p("a"), Value::Integer(1)), (p("z"), Value::Boolean(false))]).unwrap();
        let patch = Patch::new([(p("a.b"), Value::Integer(2))], vec![p("a")]).unwrap();
        let updated = d.apply(&patch).unwrap();
        assert_eq!(updated.get(&p("a.b")), Some(&Value::Integer(2)));
        assert_eq!(updated.get(&p("z")), Some(&Value::Boolean(false)));
        assert!(Patch::new([(p("a"), Value::Integer(0))], vec![p("a")]).is_err());
    }
}
