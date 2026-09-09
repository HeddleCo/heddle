//! The deployment contract is an independent completeness oracle. Adding a
//! schema method never makes an endpoint implementation appear automatically.
use std::collections::BTreeSet;

use api::heddle::api::v1alpha1::DeploymentTarget;

#[test]
fn all_device_deployment_methods_have_real_production_handlers() {
    let expected = api::v2::ALL_METHODS
        .iter()
        .filter(|method| {
            method
                .deployment_targets
                .contains(&DeploymentTarget::HeddleDaemon)
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
            "/heddle.api.v2alpha1.EndpointService/DescribeEndpoint",
            "/heddle.api.v2alpha1.OwnerAuthorizationService/PrepareAccountClaim",
            "/heddle.api.v2alpha1.OwnerAuthorizationService/SignAccountClaim",
        ])
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
    let unexpected = actual.difference(&expected).copied().collect::<Vec<_>>();
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "missing native device handlers: {missing:#?}; handlers outside deployment contract: {unexpected:#?}"
    );
}
