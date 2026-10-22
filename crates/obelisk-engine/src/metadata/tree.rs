use super::{
    codec::{Entry, Node},
    *,
};
use crate::{corrupt, format};
use bytes::Bytes;
use obelisk_storage::{ObjectId, Storage};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    ops::Bound,
    pin::Pin,
};

pub type Changes = BTreeMap<Vec<u8>, Option<Vec<u8>>>;
#[derive(Debug)]
pub struct PreparedTree {
    pub root: Option<PageRef>,
    pub objects: Vec<(ObjectId, Bytes)>,
}
impl PreparedTree {
    /// Creates immutable objects only. Publication remains the engine's job.
    pub async fn persist(&self, store: &dyn Storage) -> Result<()> {
        for (id, bytes) in &self.objects {
            store.create(*id, bytes.clone()).await?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy)]
pub struct KeyRange<'a> {
    pub start: Bound<&'a [u8]>,
    pub end: Option<&'a [u8]>,
}

fn memory(node: &Node) -> usize {
    node.size() * 2 + node.entries.len() * 64
}
async fn load(
    store: &dyn Storage,
    family: Family,
    page: &PageRef,
    budget: &mut Budget,
) -> Result<Node> {
    if page.level >= MAX_HEIGHT || page.count == 0 || page.count as usize > MAX_ENTRIES {
        return Err(corrupt("invalid page descriptor"));
    }
    charge(&mut budget.page_reads, 1, "metadata page visits")?;
    budget.charge_decoded(page.blob.len as usize * 2 + page.count as usize * 64)?;
    let bytes = page.blob.read(store, MAX_PAGE_BYTES, budget).await?;
    // The reservation above uses the descriptor count. Verify it against the
    // encoded header before the decoder allocates entry vectors.
    let encoded_count = bytes
        .get(45..49)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()));
    if encoded_count != Some(page.count) {
        return Err(corrupt("page descriptor count mismatch"));
    }
    let node = Node::decode(&bytes, family)?;
    if node.level != page.level || node.entries.len() != page.count as usize {
        return Err(corrupt("page descriptor mismatch"));
    }
    Ok(node)
}
fn bounds(node: &Node, lower: Option<&[u8]>, upper: Option<&[u8]>) -> Result<()> {
    if lower.is_some_and(|k| node.entries[0].0 != k)
        || upper.is_some_and(|k| node.entries.last().unwrap().0.as_slice() >= k)
    {
        return Err(corrupt("page separator range mismatch"));
    }
    Ok(())
}
pub async fn get(
    store: &dyn Storage,
    family: Family,
    root: Option<&PageRef>,
    key: &[u8],
    budget: &mut Budget,
) -> Result<Option<Vec<u8>>> {
    let Some(mut page) = root.cloned() else {
        return Ok(None);
    };
    let mut lower = None::<Vec<u8>>;
    let mut upper = None::<Vec<u8>>;
    loop {
        let node = load(store, family, &page, budget).await?;
        bounds(&node, lower.as_deref(), upper.as_deref())?;
        if node.level == 0 {
            return match node.entries.binary_search_by(|e| e.0.as_slice().cmp(key)) {
                Ok(i) => {
                    budget.charge_decoded(node.entries[i].1.len())?;
                    Ok(Some(node.entries[i].1.clone()))
                }
                Err(_) => Ok(None),
            };
        }
        let end = node.entries.partition_point(|e| e.0.as_slice() <= key);
        if end == 0 {
            return Ok(None);
        }
        let i = end - 1;
        if let Some(next) = node.entries.get(i + 1) {
            upper = Some(next.0.clone());
        }
        lower = Some(node.entries[i].0.clone());
        page = PageRef::from_bytes(&node.entries[i].1)?;
    }
}
/// Ordered bounded traversal. Child bounds prune subtrees before they are read.
pub async fn range(
    store: &dyn Storage,
    family: Family,
    root: Option<&PageRef>,
    query: KeyRange<'_>,
    limit: usize,
    budget: &mut Budget,
) -> Result<Vec<Entry>> {
    if limit == 0 || limit > MAX_ENTRIES {
        return Err(Error::ResourceLimit("metadata range result count"));
    }
    let Some(root) = root else {
        return Ok(Vec::new());
    };
    let mut stack = vec![(root.clone(), None::<Vec<u8>>, None::<Vec<u8>>)];
    let mut out = Vec::new();
    while let Some((page, lower, upper)) = stack.pop() {
        let node = load(store, family, &page, budget).await?;
        bounds(&node, lower.as_deref(), upper.as_deref())?;
        if node.level == 0 {
            for (key, value) in node.entries {
                let before = match query.start {
                    Bound::Unbounded => false,
                    Bound::Included(k) => key.as_slice() < k,
                    Bound::Excluded(k) => key.as_slice() <= k,
                };
                if before {
                    continue;
                }
                if query.end.is_some_and(|end| key.as_slice() >= end) {
                    return Ok(out);
                }
                budget.charge_decoded(key.len() + value.len() + 64)?;
                out.push((key, value));
                if out.len() == limit {
                    return Ok(out);
                }
            }
        } else {
            for i in (0..node.entries.len()).rev() {
                let (key, value) = &node.entries[i];
                let end = node
                    .entries
                    .get(i + 1)
                    .map(|e| e.0.as_slice())
                    .or(upper.as_deref());
                let before = match query.start {
                    Bound::Unbounded => false,
                    Bound::Included(k) | Bound::Excluded(k) => end.is_some_and(|e| e <= k),
                };
                if before || query.end.is_some_and(|end| key.as_slice() >= end) {
                    continue;
                }
                budget.charge_decoded(key.len() + end.map_or(0, <[u8]>::len) + 128)?;
                stack.push((
                    PageRef::from_bytes(value)?,
                    Some(key.clone()),
                    end.map(<[u8]>::to_vec),
                ));
            }
        }
    }
    Ok(out)
}

struct Editor<'a> {
    store: &'a dyn Storage,
    family: Family,
    budget: &'a mut Budget,
    cached: BTreeMap<ObjectId, (PageRef, Node)>,
    staged: BTreeMap<ObjectId, (Node, Bytes)>,
    order: Vec<ObjectId>,
}
impl Editor<'_> {
    async fn node(&mut self, page: &PageRef) -> Result<Node> {
        let cached = if let Some((node, bytes)) = self.staged.get(&page.blob.object) {
            if ObjectRef::new(page.blob.object, bytes) != page.blob {
                return Err(corrupt("staged reference mismatch"));
            }
            Some(node)
        } else if let Some((verified, node)) = self.cached.get(&page.blob.object) {
            if verified != page {
                return Err(corrupt("cached reference mismatch"));
            }
            Some(node)
        } else {
            None
        };
        if let Some(node) = cached {
            if node.level != page.level || node.entries.len() != page.count as usize {
                return Err(corrupt("cached descriptor mismatch"));
            }
            self.budget.charge_decoded(memory(node))?;
            return Ok(node.clone());
        }
        let node = load(self.store, self.family, page, self.budget).await?;
        self.budget.charge_decoded(memory(&node))?;
        self.cached
            .insert(page.blob.object, (page.clone(), node.clone()));
        Ok(node)
    }
    fn stage(&mut self, node: Node) -> Result<Entry> {
        self.budget.charge_decoded(memory(&node) * 2)?;
        let bytes = node.encode(self.family)?;
        charge(
            &mut self.budget.prepared,
            bytes.len(),
            "prepared metadata bytes",
        )?;
        let object = ObjectId::random();
        let reference = PageRef {
            blob: ObjectRef::new(object, &bytes),
            level: node.level,
            count: node.entries.len() as u32,
        };
        let key = node.entries[0].0.clone();
        self.staged.insert(object, (node, bytes.into()));
        self.order.push(object);
        Ok((key, reference.bytes()))
    }
    fn pack(&mut self, level: u8, entries: Vec<Entry>) -> Result<Vec<Entry>> {
        if level >= MAX_HEIGHT {
            return Err(Error::ResourceLimit("metadata height"));
        }
        let min = if level == 0 { 1 } else { 2 };
        let mut groups = Vec::<Vec<Entry>>::new();
        let mut group = Vec::new();
        let mut size = format::HEADER_BYTES + 6;
        for entry in entries {
            let cost = 8 + entry.0.len() + entry.1.len();
            if !group.is_empty()
                && group.len() >= min
                && (size + cost > PAGE_TARGET || group.len() == MAX_ENTRIES)
            {
                groups.push(std::mem::take(&mut group));
                size = format::HEADER_BYTES + 6;
            }
            size += cost;
            group.push(entry);
        }
        if !group.is_empty() {
            groups.push(group);
        }
        if level > 0 && groups.len() > 1 && groups.last().unwrap().len() == 1 {
            let tail = groups.pop().unwrap();
            let previous = groups.last_mut().unwrap();
            if previous.len() > 2 {
                let moved = previous.pop().unwrap();
                groups.push(vec![moved, tail.into_iter().next().unwrap()]);
            } else {
                previous.extend(tail);
            }
        }
        groups
            .into_iter()
            .map(|entries| self.stage(Node { level, entries }))
            .collect()
    }
    async fn rebalance(&mut self, children: &mut Vec<Entry>) -> Result<()> {
        let mut i = 0;
        while i < children.len() && children.len() > 1 {
            let page = PageRef::from_bytes(&children[i].1)?;
            // An unchanged sparse fringe is not maintenance work for this
            // transaction. Only edited pages can require sibling repair.
            if !self.staged.contains_key(&page.blob.object) {
                i += 1;
                continue;
            }
            if page.blob.len as usize >= PAGE_TARGET / 3 && (page.level == 0 || page.count >= 2) {
                i += 1;
                continue;
            }
            let left = i.saturating_sub(1);
            let right = left + 1;
            let left_ref = PageRef::from_bytes(&children[left].1)?;
            let right_ref = PageRef::from_bytes(&children[right].1)?;
            let mut a = self.node(&left_ref).await?;
            let b = self.node(&right_ref).await?;
            bounds(&a, Some(&children[left].0), Some(&children[right].0))?;
            bounds(&b, Some(&children[right].0), None)?;
            if a.level != b.level {
                return Err(corrupt("sibling level mismatch"));
            }
            a.entries.extend(b.entries);
            let replacement = self.pack(a.level, a.entries)?;
            let count = replacement.len();
            children.splice(left..=right, replacement);
            i = left + count;
        }
        Ok(())
    }
    fn edit<'a>(
        &'a mut self,
        page: Option<PageRef>,
        changes: &'a [(Vec<u8>, Option<Vec<u8>>)],
        lower: Option<Vec<u8>>,
        upper: Option<Vec<u8>>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Entry>>> + Send + 'a>> {
        Box::pin(async move {
            let node = match &page {
                Some(page) => self.node(page).await?,
                None => Node {
                    level: 0,
                    entries: Vec::new(),
                },
            };
            if page.is_some() {
                bounds(&node, lower.as_deref(), upper.as_deref())?;
            }
            self.budget.charge_decoded(memory(&node))?;
            let original = node.entries.clone();
            let mut entries;
            if node.level == 0 {
                let mut map: BTreeMap<_, _> = node.entries.into_iter().collect();
                for (key, value) in changes {
                    if let Some(value) = value {
                        map.insert(key.clone(), value.clone());
                    } else {
                        map.remove(key);
                    }
                }
                entries = map.into_iter().collect();
            } else {
                entries = Vec::new();
                let mut start = 0;
                for (i, (key, value)) in node.entries.iter().enumerate() {
                    let next = node
                        .entries
                        .get(i + 1)
                        .map(|e| e.0.clone())
                        .or_else(|| upper.clone());
                    let end = if i + 1 == node.entries.len() {
                        changes.len()
                    } else {
                        changes.partition_point(|e| e.0 < node.entries[i + 1].0)
                    };
                    if end > start {
                        let child = PageRef::from_bytes(value)?;
                        entries.extend(
                            self.edit(Some(child), &changes[start..end], Some(key.clone()), next)
                                .await?,
                        );
                    } else {
                        entries.push((key.clone(), value.clone()));
                    }
                    start = end;
                }
                self.rebalance(&mut entries).await?;
            }
            if entries == original
                && let Some(page) = page
            {
                return Ok(vec![(original[0].0.clone(), page.bytes())]);
            }
            self.pack(node.level, entries)
        })
    }
}
pub async fn prepare(
    store: &dyn Storage,
    family: Family,
    root: Option<&PageRef>,
    changes: &Changes,
    budget: &mut Budget,
) -> Result<PreparedTree> {
    if changes.len() > MAX_ENTRIES {
        return Err(Error::ResourceLimit("metadata batch entries"));
    }
    for (key, value) in changes {
        if key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || value.as_ref().is_some_and(|v| v.len() > MAX_VALUE_BYTES)
        {
            return Err(Error::ResourceLimit("metadata key/value size"));
        }
        budget.charge_decoded(key.len() + value.as_ref().map_or(0, Vec::len) + 128)?;
    }
    if changes.is_empty() {
        return Ok(PreparedTree {
            root: root.cloned(),
            objects: Vec::new(),
        });
    }
    let changes: Vec<_> = changes
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut editor = Editor {
        store,
        family,
        budget,
        cached: BTreeMap::new(),
        staged: BTreeMap::new(),
        order: Vec::new(),
    };
    let mut entries = editor.edit(root.cloned(), &changes, None, None).await?;
    while entries.len() > 1 {
        let level = PageRef::from_bytes(&entries[0].1)?.level + 1;
        entries = editor.pack(level, entries)?;
    }
    let mut root = entries
        .first()
        .map(|e| PageRef::from_bytes(&e.1))
        .transpose()?;
    while let Some(page) = &root {
        if page.level == 0 || page.count != 1 {
            break;
        }
        let node = editor.node(page).await?;
        root = Some(PageRef::from_bytes(&node.entries[0].1)?);
    }
    // Intermediate pages replaced by sibling rebalancing are not uploaded.
    let mut reachable = BTreeSet::new();
    let mut pending: Vec<_> = root.iter().cloned().collect();
    while let Some(page) = pending.pop() {
        if !reachable.insert(page.blob.object) {
            continue;
        }
        if let Some((node, _)) = editor.staged.get(&page.blob.object)
            && node.level > 0
        {
            for (_, value) in &node.entries {
                pending.push(PageRef::from_bytes(value)?);
            }
        }
    }
    let objects = editor
        .order
        .into_iter()
        .filter(|id| reachable.contains(id))
        .map(|id| {
            let (_, bytes) = editor.staged.remove(&id).unwrap();
            (id, bytes)
        })
        .collect();
    Ok(PreparedTree { root, objects })
}
