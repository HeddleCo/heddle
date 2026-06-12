// SPDX-License-Identifier: Apache-2.0
use super::*;

const V4_HEADER_LEN: usize = 8 + 4 + 4 + 8 + 8;
const FOOTER_LEN: usize = 8 + 4 + 4 + (13 * 8);
const ENTRY_OFFSET_RECORD_LEN: usize = 16;

#[test]
fn truncated_packed_oplog_salvages_prefix_quarantines_tail_and_keeps_repo_usable() {
    for case_name in ["entry-header", "mid-record", "last-record"] {
        let temp = TempDir::new().unwrap();
        seed_native_repo_with_three_captures(temp.path());

        let oplog_path = temp.path().join(".heddle/oplog/oplog.bin");
        let original = std::fs::read(&oplog_path).expect("read packed oplog");
        let (entry_offsets, entry_data_end) = current_entry_offsets(&original);
        assert!(
            entry_offsets.len() >= 6,
            "test fixture should have multiple packed oplog entries"
        );

        let case = TruncationCase::new(case_name, &entry_offsets, entry_data_end);
        let mut truncated = original.clone();
        truncated.truncate(case.truncate_at);
        std::fs::write(&oplog_path, truncated).expect("write truncated packed oplog");

        let output = heddle_output(&["log", "--output", "json"], Some(temp.path()))
            .expect("heddle log should run");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{}: log should succeed after salvage; stdout={} stderr={}",
            case.name,
            String::from_utf8_lossy(&output.stdout),
            stderr
        );
        assert!(
            stderr.contains("kind=state_corrupted")
                && stderr.contains("Packed oplog was truncated")
                && stderr.contains(&format!("recovered_records={}", case.expected_recovered))
                && stderr.contains(&format!("lost_records={}", case.expected_lost))
                && stderr.contains("damaged_byte_range="),
            "{}: recovery advice-shaped warning should describe loss: {stderr}",
            case.name
        );
        assert!(
            temp.path().join(".heddle/oplog/oplog.bin.corrupt").exists(),
            "{}: damaged packed oplog should be quarantined",
            case.name
        );

        let parsed: Value =
            serde_json::from_slice(&output.stdout).expect("log should emit JSON on stdout");
        assert!(
            !parsed["states"].as_array().unwrap().is_empty(),
            "{}: salvaged log should retain at least one complete state",
            case.name
        );

        std::fs::write(
            temp.path().join("file.txt"),
            format!("after {}\n", case.name),
        )
        .expect("modify worktree after recovery");
        let capture = heddle_output(&["capture", "-m", "after recovery"], Some(temp.path()))
            .expect("capture after recovery should run");
        assert!(
            capture.status.success(),
            "{}: repo should remain functional after recovery; stdout={} stderr={}",
            case.name,
            String::from_utf8_lossy(&capture.stdout),
            String::from_utf8_lossy(&capture.stderr)
        );
    }
}

struct TruncationCase {
    name: &'static str,
    truncate_at: usize,
    expected_recovered: usize,
    expected_lost: usize,
}

impl TruncationCase {
    fn new(name: &'static str, offsets: &[usize], entry_data_end: usize) -> Self {
        let (truncate_at, expected_recovered) = match name {
            "entry-header" => (offsets[2] + 4, 2),
            "mid-record" => (offsets[3] + ((offsets[4] - offsets[3]) / 2), 3),
            "last-record" => (
                offsets[offsets.len() - 1] + ((entry_data_end - offsets[offsets.len() - 1]) / 2),
                offsets.len() - 1,
            ),
            other => panic!("unknown truncation case {other}"),
        };
        Self {
            name,
            truncate_at,
            expected_recovered,
            expected_lost: offsets.len() - expected_recovered,
        }
    }
}

fn seed_native_repo_with_three_captures(path: &std::path::Path) {
    let init = heddle_output(&["init", "--no-harness-install"], Some(path)).expect("init runs");
    assert!(
        init.status.success(),
        "init should succeed: stdout={} stderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );
    for index in 1..=3 {
        std::fs::write(path.join("file.txt"), format!("snapshot {index}\n"))
            .expect("write snapshot file");
        let capture = heddle_output(&["capture", "-m", &format!("snapshot {index}")], Some(path))
            .expect("capture runs");
        assert!(
            capture.status.success(),
            "capture {index} should succeed: stdout={} stderr={}",
            String::from_utf8_lossy(&capture.stdout),
            String::from_utf8_lossy(&capture.stderr)
        );
    }
}

fn current_entry_offsets(bytes: &[u8]) -> (Vec<usize>, usize) {
    assert!(bytes.len() >= V4_HEADER_LEN + FOOTER_LEN);
    let footer_start = bytes.len() - FOOTER_LEN;
    assert_eq!(&bytes[footer_start..footer_start + 8], b"LMOPIDX\0");

    let entry_data_end = read_u64(bytes, footer_start + 16) as usize;
    let entry_offsets_offset = read_u64(bytes, footer_start + 24) as usize;
    let entry_offsets_count = read_u64(bytes, footer_start + 32) as usize;
    let mut offsets = Vec::with_capacity(entry_offsets_count);
    for index in 0..entry_offsets_count {
        let record = entry_offsets_offset + (index * ENTRY_OFFSET_RECORD_LEN);
        offsets.push(read_u64(bytes, record + 8) as usize);
    }
    (offsets, entry_data_end)
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(out)
}
