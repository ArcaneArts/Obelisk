use crate::{
    Document, DocumentPath, MAX_BATCH_ITEMS, MAX_TRANSACTION_BYTES, Patch, Result, invalid,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
macro_rules! identity {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);
        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err(invalid("invalid identity"));
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
identity!(RequestId);
identity!(DatabaseId);
identity!(CommitId);
identity!(Revision);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Any,
    Absent,
    Revision(Revision),
}
#[derive(Debug, Clone, PartialEq)]
pub enum MutationKind {
    Put(Document),
    Patch(Patch),
    Delete,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation {
    pub path: DocumentPath,
    pub condition: Condition,
    pub kind: MutationKind,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadCondition {
    pub path: DocumentPath,
    pub condition: Condition,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Transaction {
    request_id: RequestId,
    mutations: Vec<Mutation>,
    conditions: Vec<ReadCondition>,
}
impl Transaction {
    pub fn new(
        request_id: RequestId,
        mutations: Vec<Mutation>,
        conditions: Vec<ReadCondition>,
    ) -> Result<Self> {
        if mutations.is_empty()
            || mutations.len() > MAX_BATCH_ITEMS
            || conditions.len() > MAX_BATCH_ITEMS
        {
            return Err(invalid("invalid batch size"));
        }
        let mut seen = BTreeSet::new();
        let mut size = 0;
        for mutation in &mutations {
            if !seen.insert(&mutation.path) {
                return Err(invalid("multiple mutations for one document"));
            }
            size += mutation.path.to_string().len() + 512;
            size += match &mutation.kind {
                MutationKind::Put(d) => d.estimated_size()?,
                MutationKind::Patch(p) => {
                    Document::new(p.set().clone())?.estimated_size()?
                        + p.remove()
                            .iter()
                            .map(|p| p.to_string().len() + 4)
                            .sum::<usize>()
                }
                MutationKind::Delete => 0,
            };
        }
        size += conditions
            .iter()
            .map(|c| c.path.to_string().len() + 512)
            .sum::<usize>();
        if size > MAX_TRANSACTION_BYTES {
            return Err(invalid("transaction byte limit"));
        }
        Ok(Self {
            request_id,
            mutations,
            conditions,
        })
    }
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }
    pub fn conditions(&self) -> &[ReadCondition] {
        &self.conditions
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    pub database_id: DatabaseId,
    pub request_id: RequestId,
    pub commit_id: CommitId,
    pub sequence: u64,
    pub revisions: BTreeMap<DocumentPath, Option<Revision>>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct VersionedDocument {
    pub document: Document,
    pub revision: Revision,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_duplicate_mutations_and_invalid_ids() {
        let m = Mutation {
            path: DocumentPath::parse("a").unwrap(),
            condition: Condition::Any,
            kind: MutationKind::Delete,
        };
        assert!(
            Transaction::new(RequestId::new("r").unwrap(), vec![m.clone(), m], vec![]).is_err()
        );
        assert!(RequestId::new("").is_err());
        assert!(Transaction::new(RequestId::new("r").unwrap(), vec![], vec![]).is_err());
    }
}
