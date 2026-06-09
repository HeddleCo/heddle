// SPDX-License-Identifier: Apache-2.0
//! [`ObjectKind`] — shared object-type vocabulary.

use sley_object::ObjectType;

/// Git object kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }

    pub const fn from_sley(object_type: ObjectType) -> Self {
        match object_type {
            ObjectType::Blob => Self::Blob,
            ObjectType::Tree => Self::Tree,
            ObjectType::Commit => Self::Commit,
            ObjectType::Tag => Self::Tag,
        }
    }
}

impl From<ObjectType> for ObjectKind {
    fn from(object_type: ObjectType) -> Self {
        Self::from_sley(object_type)
    }
}