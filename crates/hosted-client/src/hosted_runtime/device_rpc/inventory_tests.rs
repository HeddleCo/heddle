//! The deployment contract is an independent completeness oracle. Adding a
//! schema method never makes an endpoint implementation appear automatically.
use std::collections::BTreeSet;

use api::heddle::api::common::DeploymentTarget;

#[test]
fn all_device_deployment_methods_have_real_production_handlers() {
    let _process_env_guard = crate::test_process_env::shared_blocking();
    let expected = api::v2::ALL_METHODS
        .iter()
        .filter(|method| {
            method
                .deployment_targets
                .contains(&DeploymentTarget::HeddleDaemon)
                && required_daemon_handler(method.path)
        })
        .map(|method| method.path)
        .collect::<BTreeSet<_>>();
    assert!(
        expected.len() > 30,
        "device deployment inventory must not become vacuous"
    );
    let actual = super::METHODS
        .iter()
        .copied()
        .chain([
            "/heddle.api.v1alpha2.EndpointService/DescribeEndpoint",
            "/heddle.api.v1alpha2.OwnerAuthorizationService/PrepareAccountClaim",
            "/heddle.api.v1alpha2.OwnerAuthorizationService/SignAccountClaim",
        ])
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
    let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "missing native device handlers: {missing:#?}; handlers outside deployment contract: {unexpected:#?}"
    );
}

fn required_daemon_handler(path: &str) -> bool {
    if path == "/heddle.api.v1alpha2.SpoolService/PromoteSpool" {
        // Promotion moves a personal child into Weft's shared root namespace.
        // The local catalog has no shared namespace to mutate, and the CLI only
        // sends this RPC outbound to Weft. The frozen contract's daemon target
        // is aspirational (heddle#1776), rather than represented by a fake handler.
        return false;
    }
    if path == "/heddle.api.v1alpha2.AnalysisService/StartAnalysis" {
        return cfg!(feature = "semantic");
    }
    true
}
