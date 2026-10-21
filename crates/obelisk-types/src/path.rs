use crate::{MAX_PATH_BYTES, Result, invalid};
use std::fmt;

fn validate(segments: &[String]) -> Result<()> {
    if segments.is_empty() || segments.len() > 128 {
        return Err(invalid("path must have 1..=128 segments"));
    }
    let mut bytes = segments.len() - 1;
    for segment in segments {
        if segment.is_empty() || segment.chars().any(char::is_control) {
            return Err(invalid("empty path segment or control character"));
        }
        // Account for worst-case escaping, so every valid path can round-trip.
        bytes += segment
            .chars()
            .map(|c| c.len_utf8() + usize::from(c == char::from(92) || c == '/' || c == '.'))
            .sum::<usize>();
    }
    if bytes > MAX_PATH_BYTES {
        return Err(invalid("path exceeds byte limit"));
    }
    Ok(())
}
fn parse(text: &str, delimiter: char) -> Result<Vec<String>> {
    if text.len() > MAX_PATH_BYTES {
        return Err(invalid("path exceeds byte limit"));
    }
    let mut parts = vec![String::new()];
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == char::from(92) {
            match chars.next() {
                Some(next) if next == delimiter || next == char::from(92) => {
                    parts.last_mut().unwrap().push(next)
                }
                _ => return Err(invalid("invalid path escape")),
            }
        } else if c == delimiter {
            parts.push(String::new());
        } else {
            parts.last_mut().unwrap().push(c);
        }
    }
    validate(&parts)?;
    Ok(parts)
}
fn display(parts: &[String], delimiter: char, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            write!(f, "{delimiter}")?;
        }
        for c in part.chars() {
            if c == delimiter || c == char::from(92) {
                write!(f, "\\")?;
            }
            write!(f, "{c}")?;
        }
    }
    Ok(())
}
macro_rules! path {
    ($name:ident, $delimiter:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Vec<String>);
        impl $name {
            pub fn parse(text: &str) -> Result<Self> {
                Ok(Self(parse(text, $delimiter)?))
            }
            pub fn from_segments<I, S>(segments: I) -> Result<Self>
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                let parts: Vec<_> = segments.into_iter().map(Into::into).collect();
                validate(&parts)?;
                Ok(Self(parts))
            }
            pub fn segments(&self) -> &[String] {
                &self.0
            }
            pub fn is_ancestor_of(&self, other: &Self) -> bool {
                self.0.len() < other.0.len() && other.0.starts_with(&self.0)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                display(&self.0, $delimiter, f)
            }
        }
        impl std::str::FromStr for $name {
            type Err = crate::ValidationError;
            fn from_str(text: &str) -> Result<Self> {
                Self::parse(text)
            }
        }
    };
}
path!(DocumentPath, '/');
path!(FieldPath, '.');

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    #[test]
    fn boundaries_and_escapes() {
        for invalid in ["", "/a", "a/", "a//b", "a\\q", "a\n"] {
            assert!(DocumentPath::parse(invalid).is_err(), "{invalid:?}");
        }
        let a = DocumentPath::parse("a/b").unwrap();
        assert!(!a.is_ancestor_of(&DocumentPath::parse("a/bc").unwrap()));
        assert!(a.is_ancestor_of(&DocumentPath::parse("a/b/c").unwrap()));
        let path = FieldPath::from_segments(["literal.dot", "back\\slash"]).unwrap();
        assert_eq!(path, FieldPath::parse(&path.to_string()).unwrap());
        assert!(DocumentPath::from_segments(vec!["a"; 129]).is_err());
        assert!(DocumentPath::parse(&"a".repeat(MAX_PATH_BYTES + 1)).is_err());
    }
    proptest! {
        #[test]
        fn structured_paths_roundtrip(parts in prop::collection::vec("[a-zA-Z./\\\\]{1,20}", 1..10)) {
            let d = DocumentPath::from_segments(parts.clone()).unwrap();
            prop_assert_eq!(&d, &DocumentPath::parse(&d.to_string()).unwrap());
            let f = FieldPath::from_segments(parts).unwrap();
            prop_assert_eq!(&f, &FieldPath::parse(&f.to_string()).unwrap());
        }
    }
}
