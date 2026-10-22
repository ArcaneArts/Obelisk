//! Immutable ordered metadata primitives for the paged engine. Tree operations
//! never publish HEAD or reclaim historical objects.
mod codec;
pub(crate) use codec::decode_outcome_budget;
mod index;
mod key;
mod tree;
pub use codec::{
    HeadRef, ObjectRef, OutcomeRef, PageRef, Root, decode_head, decode_location, decode_outcome,
    decode_root, encode_head, encode_location, encode_outcome, encode_root,
};
pub use index::{
    INDEX_DEFINITION_KIND, MAX_INDEX_SCALAR_BYTES, decode_index_definition, decode_index_entry_key,
    encode_index_definition, index_definition_id, index_definition_key, index_entry_for_document,
    index_entry_key, index_prefix_end, index_value_prefix,
};
pub use key::{
    document_key, document_path, membership_key, membership_path, parent_prefix, prefix_end,
};
pub use tree::{Changes, KeyRange, PreparedTree, get, prepare, range};

use crate::{Error, Result};

pub const PAGE_TARGET: usize = 16 * 1024;
pub const MAX_PAGE_BYTES: usize = 64 * 1024;
pub const MAX_ROOT_BYTES: usize = 16 * 1024;
pub const MAX_HEIGHT: u8 = 32;
pub const MAX_KEY_BYTES: usize = 2 * obelisk_types::MAX_PATH_BYTES + 512;
pub const MAX_VALUE_BYTES: usize = 8192;
pub const MAX_ENTRIES: usize = 4096;
pub const MAX_OUTCOME_BYTES: usize = 2 * 1024 * 1024;
pub const PAGE_KIND: u8 = 5;
pub const OUTCOME_KIND: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Family {
    Documents = 1,
    Requests = 2,
    Membership = 3,
    IndexDefinitions = 4,
    ScalarIndexes = 5,
}

/// Per-operation work limits, including pages revisited without a cache.
#[derive(Debug, Clone)]
pub struct Budget {
    pub page_reads: usize,
    pub transferred: usize,
    pub decoded: usize,
    pub prepared: usize,
}
impl Default for Budget {
    fn default() -> Self {
        Self {
            page_reads: 4096,
            transferred: 64 * 1024 * 1024,
            decoded: 64 * 1024 * 1024,
            prepared: 64 * 1024 * 1024,
        }
    }
}
impl Budget {
    pub fn charge_decoded(&mut self, bytes: usize) -> Result<()> {
        charge(&mut self.decoded, bytes, "metadata decoded memory")
    }
    pub fn charge_transfer(&mut self, bytes: usize) -> Result<()> {
        charge(&mut self.transferred, bytes, "metadata transfer bytes")
    }
}
pub(crate) fn charge(remaining: &mut usize, bytes: usize, what: &'static str) -> Result<()> {
    *remaining = remaining
        .checked_sub(bytes)
        .ok_or(Error::ResourceLimit(what))?;
    Ok(())
}

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod tests;
