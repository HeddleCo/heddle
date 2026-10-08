// SPDX-License-Identifier: Apache-2.0
//! Fixed-size sorted runs. Binary merging keeps the working set and number of
//! open files bounded even when all identities share one hash prefix.
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

use tempfile::NamedTempFile;

use crate::store::{Result, StoreError};

const SORT_BYTES: usize = 1024 * 1024;

pub(super) fn read_record<const N: usize>(reader: &mut impl Read) -> Result<Option<[u8; N]>> {
    let mut record = [0; N];
    if reader.read(&mut record[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut record[1..])?;
    Ok(Some(record))
}

pub(super) fn sort_records<const N: usize>(input: File, root: &Path) -> Result<NamedTempFile> {
    let mut input = BufReader::new(input);
    let mut levels: [Option<NamedTempFile>; 64] = std::array::from_fn(|_| None);
    let mut records = Vec::<[u8; N]>::with_capacity(SORT_BYTES / N);
    loop {
        records.clear();
        while records.len() < SORT_BYTES / N {
            let Some(record) = read_record(&mut input)? else {
                break;
            };
            records.push(record);
        }
        if records.is_empty() {
            break;
        }
        records.sort_unstable();
        let mut run = NamedTempFile::new_in(root)?;
        {
            let mut writer = BufWriter::new(run.as_file_mut());
            for record in &records {
                writer.write_all(record)?;
            }
            writer.flush()?;
        }
        let mut carried = Some(run);
        for slot in &mut levels {
            let Some(run) = carried.take() else { break };
            match slot.take() {
                Some(previous) => carried = Some(merge::<N>(previous, run, root)?),
                None => *slot = Some(run),
            }
        }
        if carried.is_some() {
            return Err(StoreError::InvalidObject(
                "disk sort exceeds addressable file size".into(),
            ));
        }
    }
    let mut result = None;
    for run in levels.into_iter().flatten() {
        result = Some(match result {
            Some(previous) => merge::<N>(previous, run, root)?,
            None => run,
        });
    }
    let mut result = match result {
        Some(run) => run,
        None => NamedTempFile::new_in(root)?,
    };
    result.as_file_mut().seek(SeekFrom::Start(0))?;
    Ok(result)
}

fn merge<const N: usize>(
    mut left: NamedTempFile,
    mut right: NamedTempFile,
    root: &Path,
) -> Result<NamedTempFile> {
    left.as_file_mut().seek(SeekFrom::Start(0))?;
    right.as_file_mut().seek(SeekFrom::Start(0))?;
    let mut left = BufReader::new(left.as_file_mut());
    let mut right = BufReader::new(right.as_file_mut());
    let mut output = NamedTempFile::new_in(root)?;
    {
        let mut writer = BufWriter::new(output.as_file_mut());
        let mut a = read_record::<N>(&mut left)?;
        let mut b = read_record::<N>(&mut right)?;
        while a.is_some() || b.is_some() {
            match (a, b) {
                (Some(record), None) => {
                    writer.write_all(&record)?;
                    a = read_record(&mut left)?;
                }
                (None, Some(record)) => {
                    writer.write_all(&record)?;
                    b = read_record(&mut right)?;
                }
                (Some(first), Some(second)) if first <= second => {
                    writer.write_all(&first)?;
                    a = read_record(&mut left)?;
                }
                (Some(_), Some(second)) => {
                    writer.write_all(&second)?;
                    b = read_record(&mut right)?;
                }
                (None, None) => break,
            }
        }
        writer.flush()?;
    }
    Ok(output)
}
