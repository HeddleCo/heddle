// SPDX-License-Identifier: Apache-2.0
//! Portable names and verified filesystem identities for digest entries.

#[cfg(feature = "fs")]
use std::{fs, io, path::Path};

pub use heddle_object_model::name_encoding::*;

/// Persist long-name identity before publishing the entry's payload. Callers
/// serialize writes with the same lock that protects the payload.
#[cfg(feature = "fs")]
pub fn write_name_entry(root: &Path, name: &str) -> io::Result<()> {
    let relative = name_path(name);
    if !is_digest_name_path(&relative) {
        return Ok(());
    }
    let entry = root.join(&relative);
    let identity = entry.join("name");
    if identity.try_exists()? {
        verify_name_entry(root, name)?;
    } else {
        crate::fs_atomic::write_file_atomic(&identity, name.as_bytes())?;
    }
    Ok(())
}

/// Point reads and deletes must verify an existing digest entry before use.
/// An absent entry is a normal missing value; an unlabelled entry is corrupt.
#[cfg(feature = "fs")]
pub fn verify_name_entry(root: &Path, name: &str) -> io::Result<()> {
    let relative = name_path(name);
    if is_digest_name_path(&relative) && root.join(&relative).try_exists()? {
        let stored = read_name_entry(root, &relative)?;
        if stored.as_deref() != Some(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "digest entry name does not match requested name",
            ));
        }
    }
    Ok(())
}

/// Decode a plain entry or verify a digest entry's exact UTF-8 name against
/// its complete path. Corrupt identities are errors, never silent omissions.
#[cfg(feature = "fs")]
pub fn read_name_entry(root: &Path, relative: &Path) -> io::Result<Option<String>> {
    if !is_digest_name_path(relative) {
        return Ok(decode_name_path(relative));
    }
    let name = fs::read_to_string(root.join(relative).join("name"))?;
    if name_path(&name) != relative {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "digest entry name does not match storage path",
        ));
    }
    Ok(Some(name))
}

#[cfg(all(test, feature = "fs"))]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn long_names_with_shared_digest_prefix_round_trip_and_verify() {
        let root = tempfile::TempDir::new().expect("entries");
        let mut prefixes = HashMap::new();
        let (left, right) = (0..100_000)
            .find_map(|index| {
                let name = format!("{}{index}", "界".repeat(330));
                let path = name_path(&name);
                let digest = path
                    .components()
                    .next()
                    .expect("digest")
                    .as_os_str()
                    .to_str()
                    .expect("ASCII");
                prefixes
                    .insert(digest[..6].to_owned(), name.clone())
                    .map(|other| (other, name))
            })
            .expect("16-bit digest prefix collision");
        assert_ne!(name_path(&left), name_path(&right));
        for name in [&left, &right] {
            write_name_entry(root.path(), name).expect("identity write");
            assert_eq!(
                read_name_entry(root.path(), &name_path(name))
                    .expect("read")
                    .as_deref(),
                Some(name.as_str())
            );
            verify_name_entry(root.path(), name).expect("verify");
        }
        fs::write(root.path().join(name_path(&left)).join("name"), &right)
            .expect("corrupt identity");
        assert!(verify_name_entry(root.path(), &left).is_err());
        assert!(write_name_entry(root.path(), &left).is_err());
    }
}
