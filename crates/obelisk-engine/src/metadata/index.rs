use super::{MAX_KEY_BYTES, MAX_VALUE_BYTES, prefix_end};
use crate::{Error, Result, corrupt, format};
use obelisk_types::{
    Document, DocumentPath, FieldPath, IndexDefinition, IndexField, IndexId, IndexOrder,
    IndexScope, MAX_COVERED_FIELDS, MAX_INDEX_FIELDS, QueryScope, ScalarType, ValidationError,
    Value,
};

pub const INDEX_DEFINITION_KIND: u8 = 7;
/// Leaves enough room for the largest ordered document key in one metadata key.
pub const MAX_INDEX_SCALAR_BYTES: usize = 4096;

fn scalar_tag(scalar_type: ScalarType) -> u8 {
    match scalar_type {
        ScalarType::Boolean => 1,
        ScalarType::Integer => 2,
        ScalarType::Double => 3,
        ScalarType::String => 4,
    }
}
fn scalar_type(tag: u8) -> Result<ScalarType> {
    match tag {
        1 => Ok(ScalarType::Boolean),
        2 => Ok(ScalarType::Integer),
        3 => Ok(ScalarType::Double),
        4 => Ok(ScalarType::String),
        _ => Err(corrupt("invalid scalar index type")),
    }
}
fn order_tag(order: IndexOrder) -> u8 {
    match order {
        IndexOrder::Ascending => 0,
        IndexOrder::Descending => 1,
    }
}
fn order(tag: u8) -> Result<IndexOrder> {
    match tag {
        0 => Ok(IndexOrder::Ascending),
        1 => Ok(IndexOrder::Descending),
        _ => Err(corrupt("invalid scalar index order")),
    }
}
fn ordered_double(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits & (1 << 63) == 0 {
        bits ^ (1 << 63)
    } else {
        !bits
    }
}
fn double_from_ordered(bits: u64) -> f64 {
    f64::from_bits(if bits & (1 << 63) == 0 {
        !bits
    } else {
        bits ^ (1 << 63)
    })
}
fn directed(bytes: &mut [u8], order: IndexOrder) {
    if order == IndexOrder::Descending {
        for byte in bytes {
            *byte = !*byte;
        }
    }
}
fn encode_component(field: &IndexField, value: &Value, out: &mut Vec<u8>) -> Result<()> {
    if !field.scalar_type().matches(value) {
        return Err(ValidationError("index value type mismatch".into()).into());
    }
    let payload_len = match value {
        Value::Boolean(_) => 1,
        Value::Integer(_) | Value::Double(_) => 8,
        Value::String(value) => {
            value.len() + value.as_bytes().iter().filter(|byte| **byte == 0).count() + 2
        }
        Value::Binary(_) | Value::List(_) => unreachable!("type checked above"),
    };
    if out
        .len()
        .checked_add(1 + payload_len)
        .is_none_or(|size| size > MAX_INDEX_SCALAR_BYTES)
    {
        return Err(Error::ResourceLimit("scalar index key bytes"));
    }
    out.push(scalar_tag(field.scalar_type()));
    let start = out.len();
    match value {
        Value::Boolean(value) => out.push(u8::from(*value)),
        Value::Integer(value) => {
            out.extend_from_slice(&((*value as u64) ^ (1 << 63)).to_be_bytes())
        }
        Value::Double(value) => out.extend_from_slice(&ordered_double(value.get()).to_be_bytes()),
        Value::String(value) => {
            for byte in value.as_bytes() {
                if *byte == 0 {
                    out.extend_from_slice(&[0, 255]);
                } else {
                    out.push(*byte);
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        Value::Binary(_) | Value::List(_) => unreachable!("type checked above"),
    }
    directed(&mut out[start..], field.order());
    Ok(())
}
fn take<'a>(bytes: &'a [u8], cursor: &mut usize, amount: usize) -> Result<&'a [u8]> {
    let end = cursor
        .checked_add(amount)
        .ok_or_else(|| corrupt("scalar index key overflow"))?;
    let value = bytes
        .get(*cursor..end)
        .ok_or_else(|| corrupt("truncated scalar index key"))?;
    *cursor = end;
    Ok(value)
}
fn decode_component(field: &IndexField, bytes: &[u8], cursor: &mut usize) -> Result<Value> {
    if scalar_type(take(bytes, cursor, 1)?[0])? != field.scalar_type() {
        return Err(corrupt("scalar index type mismatch"));
    }
    let descending = field.order() == IndexOrder::Descending;
    let fixed = |bytes: &[u8], cursor: &mut usize, amount: usize| -> Result<Vec<u8>> {
        let mut value = take(bytes, cursor, amount)?.to_vec();
        if descending {
            directed(&mut value, IndexOrder::Descending);
        }
        Ok(value)
    };
    Ok(match field.scalar_type() {
        ScalarType::Boolean => match fixed(bytes, cursor, 1)?[0] {
            0 => Value::Boolean(false),
            1 => Value::Boolean(true),
            _ => return Err(corrupt("invalid indexed boolean")),
        },
        ScalarType::Integer => {
            let ordered = u64::from_be_bytes(fixed(bytes, cursor, 8)?.try_into().unwrap());
            Value::Integer((ordered ^ (1 << 63)) as i64)
        }
        ScalarType::Double => {
            let ordered = u64::from_be_bytes(fixed(bytes, cursor, 8)?.try_into().unwrap());
            let value =
                Value::double(double_from_ordered(ordered)).map_err(|e| corrupt(&e.to_string()))?;
            if !matches!(&value, Value::Double(value) if ordered_double(value.get()) == ordered) {
                return Err(corrupt("noncanonical indexed double"));
            }
            value
        }
        ScalarType::String => {
            let mut value = Vec::new();
            loop {
                let mut byte = take(bytes, cursor, 1)?[0];
                if descending {
                    byte = !byte;
                }
                if byte != 0 {
                    value.push(byte);
                    continue;
                }
                let mut escaped = take(bytes, cursor, 1)?[0];
                if descending {
                    escaped = !escaped;
                }
                match escaped {
                    0 => break,
                    255 => value.push(0),
                    _ => return Err(corrupt("invalid indexed string escape")),
                }
            }
            Value::String(
                String::from_utf8(value).map_err(|_| corrupt("invalid indexed string UTF-8"))?,
            )
        }
    })
}

/// Encodes the first N components. The result is a range prefix when N is less
/// than the definition's component count.
pub fn index_value_prefix(definition: &IndexDefinition, values: &[Value]) -> Result<Vec<u8>> {
    if values.len() > definition.fields().len() {
        return Err(ValidationError("too many index key values".into()).into());
    }
    let mut out = Vec::new();
    for (field, value) in definition.fields().iter().zip(values) {
        encode_component(field, value, &mut out)?;
    }
    Ok(out)
}
pub fn index_prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    prefix_end(prefix)
}
pub fn index_entry_key(
    definition: &IndexDefinition,
    values: &[Value],
    path: &DocumentPath,
) -> Result<Vec<u8>> {
    if values.len() != definition.fields().len() {
        return Err(ValidationError("incomplete scalar index key".into()).into());
    }
    finish_entry_key(definition, index_value_prefix(definition, values)?, path)
}
fn finish_entry_key(
    definition: &IndexDefinition,
    mut out: Vec<u8>,
    path: &DocumentPath,
) -> Result<Vec<u8>> {
    if !definition.scope().contains(path) {
        return Err(ValidationError("document outside index scope".into()).into());
    }
    out.extend_from_slice(&super::document_key(path));
    if out.len() > MAX_KEY_BYTES {
        return Err(Error::ResourceLimit("scalar index key bytes"));
    }
    Ok(out)
}
pub fn decode_index_entry_key(
    definition: &IndexDefinition,
    key: &[u8],
) -> Result<(Vec<Value>, DocumentPath)> {
    if key.len() > MAX_KEY_BYTES {
        return Err(corrupt("scalar index key limit"));
    }
    let mut cursor = 0;
    let mut values = Vec::with_capacity(definition.fields().len());
    for field in definition.fields() {
        values.push(decode_component(field, key, &mut cursor)?);
        if cursor > MAX_INDEX_SCALAR_BYTES {
            return Err(corrupt("scalar index value limit"));
        }
    }
    let path = super::document_path(&key[cursor..])?;
    if !definition.scope().contains(&path) {
        return Err(corrupt("index entry outside definition scope"));
    }
    Ok((values, path))
}
/// Returns no entry for documents outside the scope, with a missing component,
/// or with an exact public type mismatch.
pub fn index_entry_for_document(
    definition: &IndexDefinition,
    path: &DocumentPath,
    document: &Document,
) -> Result<Option<Vec<u8>>> {
    if !definition.scope().contains(path) {
        return Ok(None);
    }
    // Eligibility is independent of encoding size: a later missing/type-mismatched
    // field omits the row even when an earlier scalar alone would be too large.
    if definition.fields().iter().any(|field| {
        document
            .get(field.path())
            .is_none_or(|value| !field.scalar_type().matches(value))
    }) {
        return Ok(None);
    }
    let mut key = Vec::new();
    for field in definition.fields() {
        encode_component(
            field,
            document.get(field.path()).expect("eligibility checked"),
            &mut key,
        )?;
    }
    finish_entry_key(definition, key, path).map(Some)
}

pub fn index_definition_key(id: &IndexId) -> Vec<u8> {
    id.as_str().as_bytes().to_vec()
}
pub fn index_definition_id(key: &[u8]) -> Result<IndexId> {
    if key.is_empty() || key.len() > 256 {
        return Err(corrupt("invalid index definition key"));
    }
    IndexId::new(std::str::from_utf8(key).map_err(|_| corrupt("invalid index definition UTF-8"))?)
        .map_err(|e| corrupt(&e.to_string()))
}
fn write_path(writer: &mut format::Writer, path: &impl PathSegments) {
    writer.path(path.segments());
}
trait PathSegments {
    fn segments(&self) -> &[String];
}
impl PathSegments for DocumentPath {
    fn segments(&self) -> &[String] {
        self.segments()
    }
}
impl PathSegments for FieldPath {
    fn segments(&self) -> &[String] {
        self.segments()
    }
}
fn read_segments(reader: &mut format::Reader<'_>) -> Result<Vec<String>> {
    let count = reader.count(128, 64)?;
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        segments.push(reader.text(obelisk_types::MAX_PATH_BYTES)?);
    }
    Ok(segments)
}
fn write_scope(writer: &mut format::Writer, scope: &IndexScope) {
    let (kind, query) = match scope {
        IndexScope::Children(query) => (0, query),
        IndexScope::Descendants(query) => (1, query),
    };
    writer.byte(kind);
    match query {
        QueryScope::Root => writer.byte(0),
        QueryScope::Path(path) => {
            writer.byte(1);
            write_path(writer, path);
        }
    }
}
fn read_scope(reader: &mut format::Reader<'_>) -> Result<IndexScope> {
    let kind = reader.byte()?;
    let query = match reader.byte()? {
        0 => QueryScope::Root,
        1 => QueryScope::Path(
            DocumentPath::from_segments(read_segments(reader)?)
                .map_err(|e| corrupt(&e.to_string()))?,
        ),
        _ => return Err(corrupt("invalid index query scope")),
    };
    match kind {
        0 => Ok(IndexScope::Children(query)),
        1 => Ok(IndexScope::Descendants(query)),
        _ => Err(corrupt("invalid index scope kind")),
    }
}
pub fn encode_index_definition(definition: &IndexDefinition) -> Result<Vec<u8>> {
    let mut writer = format::Writer::default();
    writer.text(definition.id().as_str());
    write_scope(&mut writer, definition.scope());
    writer.u32(definition.fields().len());
    for field in definition.fields() {
        write_path(&mut writer, field.path());
        writer.byte(scalar_tag(field.scalar_type()));
        writer.byte(order_tag(field.order()));
    }
    writer.u32(definition.covered_fields().len());
    for field in definition.covered_fields() {
        write_path(&mut writer, field);
    }
    let bytes = format::envelope(INDEX_DEFINITION_KIND, writer.0)?;
    if bytes.len() > MAX_VALUE_BYTES {
        return Err(Error::ResourceLimit("index definition bytes"));
    }
    decode_index_definition(&bytes)?;
    Ok(bytes)
}
pub fn decode_index_definition(bytes: &[u8]) -> Result<IndexDefinition> {
    let mut budget = 128 * 1024 * 1024;
    decode_index_definition_budget(bytes, &mut budget)
}
pub(crate) fn decode_index_definition_budget(
    bytes: &[u8],
    budget: &mut usize,
) -> Result<IndexDefinition> {
    super::lifecycle::decoded(
        bytes,
        INDEX_DEFINITION_KIND,
        1,
        MAX_VALUE_BYTES,
        budget,
        |reader| {
            let id = IndexId::new(reader.text(256)?)?;
            let scope = read_scope(reader)?;
            let count = reader.count(MAX_INDEX_FIELDS, 128)?;
            let mut fields = Vec::with_capacity(count);
            for _ in 0..count {
                fields.push(IndexField::new(
                    FieldPath::from_segments(read_segments(reader)?)?,
                    scalar_type(reader.byte()?)?,
                    order(reader.byte()?)?,
                ));
            }
            let count = reader.count(MAX_COVERED_FIELDS, 128)?;
            let mut covered = Vec::with_capacity(count);
            for _ in 0..count {
                covered.push(FieldPath::from_segments(read_segments(reader)?)?);
            }
            if covered.windows(2).any(|pair| pair[0] >= pair[1]) {
                return Err(corrupt("noncanonical covered field order"));
            }
            Ok(IndexDefinition::new(id, scope, fields, covered)?)
        },
    )
}
