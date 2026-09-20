//! Signed annotation metadata and portable, bounded query semantics.
use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use super::{
    CollaborationCodecError, CollaborationMention, CollaborationScope, CollaborationSourceAnchor,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationDecimal {
    pub coefficient: i64,
    pub scale: u32,
}
impl AnnotationDecimal {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        if self.scale > 9 || (self.scale > 0 && self.coefficient % 10 == 0) {
            return Err(invalid(
                "decimal must be normalized with scale at most nine",
            ));
        }
        Ok(())
    }
    fn scaled(&self) -> i128 {
        i128::from(self.coefficient) * 10_i128.pow(9_u32.saturating_sub(self.scale))
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "snake_case",
    tag = "kind",
    content = "value",
    deny_unknown_fields
)]
pub enum AnnotationValue {
    Text(String),
    Boolean(bool),
    Integer(i64),
    Decimal(AnnotationDecimal),
}
impl AnnotationValue {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        match self {
            Self::Text(value) if value.len() > 512 => {
                Err(invalid("property text exceeds 512 bytes"))
            }
            Self::Decimal(value) => value.validate(),
            _ => Ok(()),
        }
    }
    fn compare(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Integer(a), Self::Integer(b)) => Some(a.cmp(b)),
            (Self::Decimal(a), Self::Decimal(b)) => Some(a.scaled().cmp(&b.scaled())),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationSourceReference {
    pub scope: CollaborationScope,
    pub source: CollaborationSourceAnchor,
}
impl AnnotationSourceReference {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        if self.scope.spool.is_nil() {
            return Err(invalid("source reference requires a spool"));
        }
        self.source.validate()?;
        path(&self.source.path)?;
        if self.source.start_line.is_some() != self.source.end_line.is_some() {
            return Err(invalid("line reference requires both inclusive endpoints"));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum AnnotationTag {
    Text {
        text: String,
    },
    Symbol {
        name: String,
        target: Option<AnnotationSourceReference>,
    },
    Source {
        target: AnnotationSourceReference,
    },
    Entity {
        target: CollaborationMention,
    },
    Property {
        key: String,
        value: AnnotationValue,
    },
}
impl From<String> for AnnotationTag {
    fn from(text: String) -> Self {
        Self::Text { text }
    }
}
impl From<&str> for AnnotationTag {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl AnnotationTag {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        match self {
            Self::Text { text: value } => text(value, 512),
            Self::Symbol { name, target } => {
                text(name, 512)?;
                if let Some(target) = target {
                    target.validate()?;
                }
                Ok(())
            }
            Self::Source { target } => target.validate(),
            Self::Entity { target } => target.validate(),
            Self::Property { key, value } => {
                property_key(key)?;
                value.validate()
            }
        }
    }
    fn source(&self) -> Option<&AnnotationSourceReference> {
        match self {
            Self::Source { target }
            | Self::Symbol {
                target: Some(target),
                ..
            } => Some(target),
            _ => None,
        }
    }
}
pub fn validate_annotation_tags(tags: &[AnnotationTag]) -> Result<(), CollaborationCodecError> {
    if tags.len() > 128 {
        return Err(invalid("at most 128 annotation tags are allowed"));
    }
    for (i, tag) in tags.iter().enumerate() {
        tag.validate()?;
        if let AnnotationTag::Property { key, .. } = tag
            && tags[..i].iter().any(|previous| matches!(previous, AnnotationTag::Property { key: other, .. } if key == other))
        {
            return Err(invalid("property keys must be unique within one revision"));
        }
    }
    Ok(())
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationComparison {
    Equal,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
    Exists,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum AnnotationTagPredicate {
    Exact {
        tag: AnnotationTag,
    },
    SymbolName {
        name: String,
    },
    FilePath {
        path: String,
    },
    FilePrefix {
        path: String,
    },
    LinesOverlap {
        target: AnnotationSourceReference,
    },
    Property {
        key: String,
        comparison: AnnotationComparison,
        value: Option<AnnotationValue>,
    },
}
impl AnnotationTagPredicate {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        match self {
            Self::Exact { tag } => tag.validate(),
            Self::SymbolName { name } => text(name, 512),
            Self::FilePath { path: value } | Self::FilePrefix { path: value } => path(value),
            Self::LinesOverlap { target } => {
                target.validate()?;
                if target.source.start_line.is_none() {
                    return Err(invalid("overlap requires a line range"));
                }
                Ok(())
            }
            Self::Property {
                key,
                comparison,
                value,
            } => {
                property_key(key)?;
                if (*comparison == AnnotationComparison::Exists) != value.is_none() {
                    return Err(invalid("only EXISTS omits a property value"));
                }
                if let Some(value) = value {
                    value.validate()?;
                    if *comparison != AnnotationComparison::Equal
                        && !matches!(
                            value,
                            AnnotationValue::Integer(_) | AnnotationValue::Decimal(_)
                        )
                    {
                        return Err(invalid("ordering requires an integer or decimal"));
                    }
                }
                Ok(())
            }
        }
    }
    fn matches(&self, tag: &AnnotationTag) -> bool {
        match self {
            Self::Exact { tag: expected } => tag == expected,
            Self::SymbolName { name } => {
                matches!(tag, AnnotationTag::Symbol { name: actual, .. } if actual == name)
            }
            Self::FilePath { path } => tag.source().is_some_and(|r| r.source.path == *path),
            Self::FilePrefix { path } => tag.source().is_some_and(|r| {
                r.source.path == *path
                    || r.source
                        .path
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('/'))
            }),
            Self::LinesOverlap { target } => tag.source().is_some_and(|r| {
                r.scope.spool == target.scope.spool
                    && target
                        .scope
                        .thread
                        .is_none_or(|thread| r.scope.thread == Some(thread))
                    && r.source.revision == target.source.revision
                    && r.source.path == target.source.path
                    && match (
                        r.source.start_line,
                        r.source.end_line,
                        target.source.start_line,
                        target.source.end_line,
                    ) {
                        (Some(a), Some(b), Some(c), Some(d)) => a <= d && c <= b,
                        _ => false,
                    }
            }),
            Self::Property {
                key,
                comparison,
                value,
            } => {
                let AnnotationTag::Property {
                    key: actual,
                    value: actual_value,
                } = tag
                else {
                    return false;
                };
                if key != actual {
                    return false;
                }
                let Some(value) = value else {
                    return *comparison == AnnotationComparison::Exists;
                };
                match comparison {
                    AnnotationComparison::Equal => actual_value == value,
                    AnnotationComparison::Less => {
                        actual_value.compare(value) == Some(Ordering::Less)
                    }
                    AnnotationComparison::LessOrEqual => matches!(
                        actual_value.compare(value),
                        Some(Ordering::Less | Ordering::Equal)
                    ),
                    AnnotationComparison::Greater => {
                        actual_value.compare(value) == Some(Ordering::Greater)
                    }
                    AnnotationComparison::GreaterOrEqual => matches!(
                        actual_value.compare(value),
                        Some(Ordering::Greater | Ordering::Equal)
                    ),
                    AnnotationComparison::Exists => false,
                }
            }
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationQuery {
    pub all: Vec<AnnotationTagPredicate>,
    pub any: Vec<AnnotationTagPredicate>,
    pub none: Vec<AnnotationTagPredicate>,
}
impl AnnotationQuery {
    pub fn validate(&self) -> Result<(), CollaborationCodecError> {
        if self
            .all
            .len()
            .saturating_add(self.any.len())
            .saturating_add(self.none.len())
            > 64
        {
            return Err(invalid("annotation query exceeds 64 predicates"));
        }
        for predicate in self.all.iter().chain(&self.any).chain(&self.none) {
            predicate.validate()?;
        }
        Ok(())
    }
    /// Matching is only defined for validated queries and validated signed tags.
    pub fn matches(&self, tags: &[AnnotationTag]) -> bool {
        let found = |p: &AnnotationTagPredicate| tags.iter().any(|tag| p.matches(tag));
        self.all.iter().all(found)
            && (self.any.is_empty() || self.any.iter().any(found))
            && !self.none.iter().any(found)
    }
}
fn property_key(value: &str) -> Result<(), CollaborationCodecError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-/".contains(&b))
    {
        return Err(invalid(
            "property key must contain 1–128 ASCII letters, digits, _, ., -, or /",
        ));
    }
    Ok(())
}
fn text(value: &str, limit: usize) -> Result<(), CollaborationCodecError> {
    if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control) {
        return Err(invalid("invalid annotation tag text"));
    }
    Ok(())
}
fn path(value: &str) -> Result<(), CollaborationCodecError> {
    text(value, 4096)?;
    if value.contains(['\\', ':']) || value.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err(invalid("source reference needs a canonical relative path"));
    }
    Ok(())
}
fn invalid(message: &str) -> CollaborationCodecError {
    CollaborationCodecError::Invalid(message.into())
}

#[cfg(test)]
#[path = "tags_tests.rs"]
mod tests;
