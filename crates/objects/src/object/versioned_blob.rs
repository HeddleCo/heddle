// SPDX-License-Identifier: Apache-2.0
//! Shared boilerplate for the versioned msgpack container blobs.
//!
//! The state-attached container blobs (structured conflicts,
//! discussions, risk signals, review signatures, context annotations)
//! all wrap a `Vec<Item>` behind an identical `format_version` byte +
//! `rmp-serde` encode/decode + version-check prologue. The only things
//! that differ are the concrete types, the field name, the error enum,
//! the `format_version` constant, and which codec-error variant the
//! enum spells. [`versioned_msgpack_blob!`] emits the common four
//! methods (`new` / `encode` / `decode` / `validate`); each blob keeps
//! only its per-item `Item::validate()` logic.

/// Emit `new` / `encode` / `decode` / `validate` for a versioned
/// msgpack container blob. The generated `validate` rejects any
/// `format_version` other than the declared one, then defers to each
/// item's own `validate()`. `decode` runs `validate` after
/// deserializing, so a stale-version blob is rejected on read.
macro_rules! versioned_msgpack_blob {
    (
        blob: $Blob:ty,
        item: $Item:ty,
        field: $field:ident,
        error: $Err:ty,
        codec_err: $codec:ident,
        version: $ver:expr $(,)?
    ) => {
        impl $Blob {
            pub const FORMAT_VERSION: u8 = $ver;

            pub fn new($field: ::std::vec::Vec<$Item>) -> Self {
                Self {
                    format_version: Self::FORMAT_VERSION,
                    $field,
                }
            }

            pub fn encode(&self) -> ::core::result::Result<::std::vec::Vec<u8>, $Err> {
                rmp_serde::to_vec(self).map_err(|err| <$Err>::$codec(err.to_string()))
            }

            pub fn decode(bytes: &[u8]) -> ::core::result::Result<Self, $Err> {
                let blob: Self =
                    rmp_serde::from_slice(bytes).map_err(|err| <$Err>::$codec(err.to_string()))?;
                blob.validate()?;
                Ok(blob)
            }

            pub fn validate(&self) -> ::core::result::Result<(), $Err> {
                if self.format_version != Self::FORMAT_VERSION {
                    return Err(<$Err>::UnsupportedVersion(self.format_version));
                }
                for item in &self.$field {
                    item.validate()?;
                }
                Ok(())
            }
        }
    };
}
