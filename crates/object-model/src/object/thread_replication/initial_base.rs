//! Canonical synthetic empty base for a new native Spool.
use super::{ThreadGenesis, invalid};
use crate::{error::Result, object::State};

/// Stable synthetic pre-history for every new Spool. This is system
/// initialization, never an assertion of human capture authorship.
pub fn synthetic_initial_base() -> Result<State> {
    use crate::object::{Attribution, ChangeId, Principal, Tree};
    let mut state = State::new_refresh_of(
        Tree::new().hash(),
        Vec::new(),
        Attribution::human(Principal::new("Heddle", "init@heddle")),
        ChangeId::from_bytes(*b"heddle-seed-v2!!"),
    );
    state.created_at = chrono::DateTime::UNIX_EPOCH;
    // Restore the derived cached ID after fixing the canonical creation time.
    State::decode_current_msgpack(&state.encode_current_msgpack()?)
}

/// Only the exact deterministic empty seed can bootstrap without an original
/// source operation. A random empty State, even with Heddle attribution, is
/// authored content and requires ordinary source provenance.
pub fn initial_base_state(genesis: &ThreadGenesis, bytes: &[u8]) -> Result<State> {
    if bytes.is_empty() || bytes.len() > 4096 {
        return Err(invalid("initial Thread base exceeds bootstrap bound"));
    }
    let state = State::decode_current_msgpack(bytes)?;
    let expected = synthetic_initial_base()?;
    if state.id() != genesis.base || expected.encode_current_msgpack()? != bytes {
        return Err(invalid(
            "initial Thread base differs from signed empty seed",
        ));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::object::{Attribution, ContentHash, Principal, Tree};
    #[test]
    fn import_initial_base_is_exact_bounded_empty_seed() {
        let seed = synthetic_initial_base().expect("known system seed");
        let mut genesis = ThreadGenesis {
            version: 1,
            spool: Uuid::from_u128(1).to_string(),
            parent: None,
            base: seed.id(),
            name: "main".into(),
            intent: String::new(),
            owner: super::super::GenesisOwner::LocalKey([1; 32]),
            creator: [1; 32],
            nonce: vec![],
        };
        let bytes = seed.encode_current_msgpack().expect("seed");
        genesis.base = seed.id();
        assert_eq!(
            initial_base_state(&genesis, &bytes)
                .expect("one-call bootstrap")
                .id(),
            seed.id()
        );
        let random_seed = State::new_snapshot(
            Tree::new().hash(),
            vec![],
            Attribution::human(Principal::new("Heddle", "init@heddle")),
        );
        genesis.base = random_seed.id();
        assert!(
            initial_base_state(
                &genesis,
                &random_seed.encode_current_msgpack().expect("random seed")
            )
            .is_err(),
            "the old random empty seed shape is not a bootstrap exception"
        );
        genesis.base = seed.id();
        let mut changed = seed.clone();
        changed.tree = ContentHash::from_bytes([88; 32]);
        genesis.base = changed.id();
        assert!(
            initial_base_state(
                &genesis,
                &changed.encode_current_msgpack().expect("changed")
            )
            .is_err(),
            "nonempty source needs authorized closure transfer"
        );
        changed = seed.clone();
        changed.provenance = Some(ContentHash::from_bytes([89; 32]));
        genesis.base = changed.id();
        assert!(
            initial_base_state(
                &genesis,
                &changed.encode_current_msgpack().expect("changed")
            )
            .is_err(),
            "seed cannot introduce another reference"
        );
        assert!(
            initial_base_state(&genesis, &bytes).is_err(),
            "base must match signed identity"
        );
    }
    #[test]
    fn synthetic_initial_base_is_stable_across_rust_and_browser() {
        let state = synthetic_initial_base().expect("synthetic seed");
        let bytes = state.encode_current_msgpack().expect("canonical seed");
        let expected = include_str!("../../../tests/fixtures/synthetic-initial-base-v2.txt");
        assert_eq!(
            format!(
                "canonical={}\nid={}\n",
                hex::encode(&bytes),
                hex::encode(state.id().as_bytes())
            ),
            expected
        );
        assert_eq!(
            bytes,
            synthetic_initial_base()
                .expect("repeat")
                .encode_current_msgpack()
                .expect("repeat bytes")
        );
        let mut genesis = ThreadGenesis {
            version: 1,
            spool: Uuid::from_u128(1).to_string(),
            parent: None,
            base: state.id(),
            name: "main".into(),
            intent: String::new(),
            owner: super::super::GenesisOwner::LocalKey([1; 32]),
            creator: [1; 32],
            nonce: vec![],
        };
        genesis.base = state.id();
        initial_base_state(&genesis, &bytes).expect("accepted seed shape");
    }
}
