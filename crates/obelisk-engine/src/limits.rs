use crate::{Result, metadata::Budget};
use obelisk_types::ValidationError;

/// Validated operation limits. Hard format limits are independent of tuning.
#[derive(Debug, Clone)]
pub struct ResourceLimits {
    pub metadata_page_reads: usize,
    pub metadata_bytes: usize,
    pub decoded_bytes: usize,
    pub prepared_metadata_bytes: usize,
}
impl Default for ResourceLimits {
    fn default() -> Self {
        let b = Budget::default();
        Self {
            metadata_page_reads: b.page_reads,
            metadata_bytes: b.transferred,
            decoded_bytes: b.decoded,
            prepared_metadata_bytes: b.prepared,
        }
    }
}
impl ResourceLimits {
    pub fn validate(&self) -> Result<()> {
        let hard = Self::default();
        for (value, max) in [
            (self.metadata_page_reads, hard.metadata_page_reads),
            (self.metadata_bytes, hard.metadata_bytes),
            (self.decoded_bytes, hard.decoded_bytes),
            (self.prepared_metadata_bytes, hard.prepared_metadata_bytes),
        ] {
            if value == 0 || value > max {
                return Err(ValidationError("invalid resource limits".into()).into());
            }
        }
        Ok(())
    }
    pub(crate) fn budget(&self) -> Budget {
        Budget {
            page_reads: self.metadata_page_reads,
            transferred: self.metadata_bytes,
            decoded: self.decoded_bytes,
            prepared: self.prepared_metadata_bytes,
        }
    }
}
