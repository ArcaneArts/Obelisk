use crate::{MAX_DEPTH, MAX_DOCUMENT_BYTES, MAX_LIST_ITEMS, Result, invalid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueType {
    Binary,
    String,
    Integer,
    Double,
    Boolean,
    List(Box<ValueType>),
}
impl ValueType {
    pub fn validate(&self) -> Result<()> {
        let mut current = self;
        let mut depth = 0;
        while let Self::List(inner) = current {
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(invalid("type nesting limit"));
            }
            current = inner;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FiniteDouble(f64);
impl FiniteDouble {
    pub fn new(value: f64) -> Result<Self> {
        if !value.is_finite() {
            return Err(invalid("double must be finite"));
        }
        Ok(Self(if value == 0.0 { 0.0 } else { value }))
    }
    pub fn get(self) -> f64 {
        self.0
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct TypedList {
    element_type: ValueType,
    values: Vec<Value>,
}
impl TypedList {
    pub fn new(element_type: ValueType, values: Vec<Value>) -> Result<Self> {
        element_type.validate()?;
        if values.len() > MAX_LIST_ITEMS {
            return Err(invalid("list length limit"));
        }
        let list = Self {
            element_type,
            values,
        };
        list.size_at(0)?;
        Ok(list)
    }
    fn size_at(&self, depth: usize) -> Result<usize> {
        let mut declared = &self.element_type;
        let mut total_depth = depth + 1;
        while let ValueType::List(inner) = declared {
            total_depth += 1;
            declared = inner;
        }
        if total_depth > MAX_DEPTH {
            return Err(invalid("list type nesting limit"));
        }
        if self.values.len() > MAX_LIST_ITEMS {
            return Err(invalid("list length limit"));
        }
        let mut size = 40;
        for value in &self.values {
            let item_size = value.size_at(depth + 1)?;
            if value.value_type() != self.element_type {
                return Err(invalid("heterogeneous list"));
            }
            size += item_size;
            if size > MAX_DOCUMENT_BYTES {
                return Err(invalid("value byte limit"));
            }
        }
        Ok(size)
    }
    pub fn element_type(&self) -> &ValueType {
        &self.element_type
    }
    pub fn values(&self) -> &[Value] {
        &self.values
    }
}
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Binary(Vec<u8>),
    String(String),
    Integer(i64),
    Double(FiniteDouble),
    Boolean(bool),
    List(TypedList),
}
impl Value {
    pub fn double(value: f64) -> Result<Self> {
        Ok(Self::Double(FiniteDouble::new(value)?))
    }
    pub fn value_type(&self) -> ValueType {
        match self {
            Self::Binary(_) => ValueType::Binary,
            Self::String(_) => ValueType::String,
            Self::Integer(_) => ValueType::Integer,
            Self::Double(_) => ValueType::Double,
            Self::Boolean(_) => ValueType::Boolean,
            Self::List(l) => ValueType::List(Box::new(l.element_type.clone())),
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.size_at(0).map(|_| ())
    }
    pub fn estimated_size(&self) -> Result<usize> {
        self.size_at(0)
    }
    fn size_at(&self, depth: usize) -> Result<usize> {
        if depth > MAX_DEPTH {
            return Err(invalid("value nesting limit"));
        }
        let size = match self {
            Self::Binary(v) => 5 + v.len(),
            Self::String(v) => 5 + v.len(),
            Self::Integer(_) | Self::Double(_) => 9,
            Self::Boolean(_) => 2,
            Self::List(list) => list.size_at(depth)?,
        };
        if size > MAX_DOCUMENT_BYTES {
            return Err(invalid("value byte limit"));
        }
        Ok(size)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_empty_and_nested_lists() {
        let empty = TypedList::new(ValueType::Integer, vec![]).unwrap();
        assert_eq!(empty.element_type(), &ValueType::Integer);
        assert!(TypedList::new(ValueType::Integer, vec![Value::String("x".into())]).is_err());
        let nested = TypedList::new(
            ValueType::List(Box::new(ValueType::Integer)),
            vec![Value::List(empty)],
        )
        .unwrap();
        assert_eq!(nested.values().len(), 1);
        assert!(
            TypedList::new(
                ValueType::Boolean,
                vec![Value::Boolean(true); MAX_LIST_ITEMS + 1]
            )
            .is_err()
        );
    }
    #[test]
    fn nesting_limit_includes_declared_empty_list_types() {
        let mut element_type = ValueType::Integer;
        for _ in 0..MAX_DEPTH {
            assert!(TypedList::new(element_type.clone(), vec![]).is_ok());
            element_type = ValueType::List(Box::new(element_type));
        }
        assert!(TypedList::new(element_type, vec![]).is_err());
    }
    #[test]
    fn double_rules() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(Value::double(v).is_err());
        }
        assert_eq!(FiniteDouble::new(-0.0).unwrap().get().to_bits(), 0);
        assert_ne!(Value::Integer(1), Value::double(1.0).unwrap());
    }
}
