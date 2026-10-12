//! Validate complete authenticated discovery while retaining only metadata and selections.
use api::{heddle::api::v1alpha2 as wire, hybrid_codec::Reject, import_authority as authority};

use super::super::Result;

pub(super) struct ImportDiscovery {
    validator: authority::DiscoveryValidator,
    source: wire::ProviderRepository,
    selected: Vec<(String, bool)>,
    mode: authority::DiscoverySelectionMode,
    exhausted: bool,
}

impl ImportDiscovery {
    pub(super) fn new(
        source: &wire::ProviderRepository,
        selected: &[wire::ImportBranchLimitV1],
        options: authority::DiscoveryOptions,
        mode: authority::DiscoverySelectionMode,
    ) -> Result<Self> {
        let validator = authority::DiscoveryValidator::new(source, true, selected, options, mode)?;
        let mut source = source.clone();
        source.refs.clear();
        source.refs_status = None;
        Ok(Self {
            validator,
            source,
            selected: selected
                .iter()
                .map(|b| (b.ref_name.clone(), false))
                .collect(),
            mode,
            exhausted: false,
        })
    }

    pub(super) fn push_page(&mut self, source: &wire::ProviderRepository) -> Result<Vec<u8>> {
        if self.exhausted
            || source.connection != self.source.connection
            || source.provider_repository_id != self.source.provider_repository_id
            || source.clone_url != self.source.clone_url
            || source.installation_id != self.source.installation_id
            || source.private != self.source.private
            || source.hash_algorithm != self.source.hash_algorithm
        {
            return Err(Reject::SourceSelection.into());
        }
        let status = source.refs_status.as_ref().ok_or(Reject::SourceSelection)?;
        let page = status.page.as_ref().ok_or(Reject::SourceSelection)?;
        if status.section != "provider_refs"
            || !matches!(
                wire::Coverage::try_from(status.coverage),
                Ok(wire::Coverage::Partial | wire::Coverage::Complete)
            )
            || (page.exhausted
                && (status.coverage != wire::Coverage::Complete as i32
                    || !page.next_page.is_empty()))
            || (!page.exhausted && (page.next_page.is_empty() || source.refs.is_empty()))
        {
            return Err(Reject::SourceSelection.into());
        }
        self.validator.push_page(&source.refs)?;
        for r in &source.refs {
            if let Ok(i) = self
                .selected
                .binary_search_by(|(name, _)| name.cmp(&r.name))
            {
                self.selected[i].1 = true;
            }
        }
        self.exhausted = page.exhausted;
        Ok(page.next_page.clone())
    }

    pub(super) fn finish(self) -> Result<(wire::ProviderRepository, authority::DiscoveryDigest)> {
        if !self.exhausted {
            return Err(Reject::SourceSelection.into());
        }
        // Prepare requires advertised selections. Commit may keep a frozen pin
        // whose ref has since been deleted upstream.
        if self.mode == authority::DiscoverySelectionMode::Prepare
            && self.selected.iter().any(|(_, seen)| !seen)
        {
            return Err(Reject::RefPinning.into());
        }
        Ok((self.source, self.validator.finish()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    fn scope() -> wire::ImportPermissionScopeV1 {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../thread-api/tests/fixtures/hybrid-alpha33.json"
        )))
        .expect("fixture");
        let bytes = hex::decode(
            fixture["wire_vectors"]["scope"]["wire_hex"]
                .as_str()
                .expect("wire"),
        )
        .expect("hex");
        wire::ImportPermissionScopeV1::decode(bytes.as_slice()).expect("scope")
    }

    fn page(refs: Vec<wire::ProviderRef>, exhausted: bool) -> wire::ProviderRepository {
        wire::ProviderRepository {
            clone_url: "https://github.com/example/repo.git".into(),
            hash_algorithm: 1,
            refs,
            refs_status: Some(wire::SectionStatus {
                section: "provider_refs".into(),
                coverage: if exhausted {
                    wire::Coverage::Complete
                } else {
                    wire::Coverage::Partial
                } as i32,
                page: Some(wire::PageInfo {
                    exhausted,
                    next_page: if exhausted { vec![] } else { vec![7] },
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn advertisement_above_4096_streams_without_retaining_refs() {
        let source = page(vec![], true);
        let mut discovery = ImportDiscovery::new(
            &source,
            &[],
            authority::DiscoveryOptions::default(),
            authority::DiscoverySelectionMode::Prepare,
        )
        .expect("validator");
        for start in (0..5000).step_by(authority::MAX_IMPORT_SOURCE_REF_PAGE) {
            let end = (start + authority::MAX_IMPORT_SOURCE_REF_PAGE).min(5000);
            let refs = (start..end)
                .map(|i| wire::ProviderRef {
                    name: format!("refs/heads/branch-{i:06}"),
                    head_oid: "11".repeat(20),
                    hash_algorithm: 1,
                    ..Default::default()
                })
                .collect();
            discovery.push_page(&page(refs, end == 5000)).expect("page");
        }
        let (metadata, digest) = discovery.finish().expect("complete discovery");
        assert_eq!(digest.ref_count(), 5000);
        assert!(metadata.refs.is_empty());
    }

    #[test]
    fn missing_selection_is_refused_before_prepare_but_frozen_commit_survives() {
        let scope = scope();
        let source = page(vec![], true);
        for mode in [
            authority::DiscoverySelectionMode::Prepare,
            authority::DiscoverySelectionMode::Commit,
        ] {
            let mut discovery = ImportDiscovery::new(
                &source,
                &scope.branches,
                authority::DiscoveryOptions::default(),
                mode,
            )
            .expect("validator");
            discovery.push_page(&source).expect("empty upstream");
            assert_eq!(
                discovery.finish().is_ok(),
                mode == authority::DiscoverySelectionMode::Commit
            );
        }
        let refs = scope
            .branches
            .iter()
            .map(|b| wire::ProviderRef {
                name: b.ref_name.clone(),
                head_oid: hex::encode(&b.pinned_commit_oid),
                hash_algorithm: b.hash_algorithm,
                ..Default::default()
            })
            .collect();
        let source = page(refs, true);
        let mut discovery = ImportDiscovery::new(
            &source,
            &scope.branches,
            authority::DiscoveryOptions::default(),
            authority::DiscoverySelectionMode::Prepare,
        )
        .expect("validator");
        discovery
            .push_page(&source)
            .expect("advertised selected pins");
        discovery.finish().expect("advertised selections pass");
    }

    #[test]
    fn incomplete_or_changed_discovery_cannot_finish() {
        let source = page(vec![], true);
        let mut discovery = ImportDiscovery::new(
            &source,
            &[],
            authority::DiscoveryOptions::default(),
            authority::DiscoverySelectionMode::Prepare,
        )
        .expect("validator");
        let mut changed = source.clone();
        changed.hash_algorithm = 2;
        assert!(discovery.push_page(&changed).is_err());
        assert!(discovery.finish().is_err());
    }

    #[test]
    fn pull_request_heads_are_opt_in_evidence_and_never_signed_selections() {
        assert!(!wire::ResolveImportSourceRequest::default().include_pull_request_refs);
        let source = page(
            vec![wire::ProviderRef {
                name: "refs/pull/12/head".into(),
                head_oid: "11".repeat(20),
                hash_algorithm: 1,
                kind: 1, // An untrusted hint cannot turn a PR head into a branch.
            }],
            true,
        );
        let mut digests = Vec::new();
        for include_pull_request_refs in [false, true] {
            let mut discovery = ImportDiscovery::new(
                &source,
                &[],
                authority::DiscoveryOptions {
                    include_pull_request_refs,
                },
                authority::DiscoverySelectionMode::Prepare,
            )
            .expect("validator");
            discovery.push_page(&source).expect("discovery evidence");
            digests.push(*discovery.finish().expect("complete").1.digest());
            let mut selected = scope().branches;
            selected.truncate(1);
            selected[0].ref_name = "refs/pull/12/head".into();
            assert!(
                ImportDiscovery::new(
                    &source,
                    &selected,
                    authority::DiscoveryOptions {
                        include_pull_request_refs
                    },
                    authority::DiscoverySelectionMode::Prepare
                )
                .is_err()
            );
        }
        assert_ne!(digests[0], digests[1]);
    }
}
