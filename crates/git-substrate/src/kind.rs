// SPDX-License-Identifier: Apache-2.0
//! [`ObjectKind`] — shared object-type vocabulary across sley and gix.

use sley_object::ObjectType;

/// Git object kind shared by sley and gix callers.
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

    #[cfg(feature = "gix-interop")]
    pub const fn from_gix(kind: gix::objs::Kind) -> Self {
        match kind {
            gix::objs::Kind::Blob => Self::Blob,
            gix::objs::Kind::Tree => Self::Tree,
            gix::objs::Kind::Commit => Self::Commit,
            gix::objs::Kind::Tag => Self::Tag,
        }
    }

    #[cfg(feature = "gix-interop")]
    pub const fn to_gix(self) -> gix::objs::Kind {
        match self {
            Self::Blob => gix::objs::Kind::Blob,
            Self::Tree => gix::objs::Kind::Tree,
            Self::Commit => gix::objs::Kind::Commit,
            Self::Tag => gix::objs::Kind::Tag,
        }
    }
}

impl From<ObjectType> for ObjectKind {
    fn from(object_type: ObjectType) -> Self {
        Self::from_sley(object_type)
    }
}