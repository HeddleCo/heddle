use super::identity::{EnsureAction, claim_link_url, ensure_action};

#[test]
fn create_on_behalf_stores_the_client_minted_root() {
    let source = include_str!("identity.rs");
    assert!(
        !source.contains("agent_capability"),
        "CreateAgentAccount must store the locally minted root, not a weft-minted capability"
    );
    assert!(
        source.contains("mint_agent_root"),
        "CreateAgentAccount must reuse the independent-root mint"
    );
}

#[test]
fn invite_is_never_selected_when_any_credential_exists() {
    assert_eq!(ensure_action(Some(true), true, false), EnsureAction::Reuse);
    assert_eq!(
        ensure_action(Some(false), true, false),
        EnsureAction::Derive
    );
    assert_eq!(ensure_action(Some(false), true, true), EnsureAction::Reuse);
}

#[test]
fn provisioning_is_only_the_no_credential_fallback() {
    assert_eq!(ensure_action(None, true, false), EnsureAction::Provision);
    assert_eq!(
        ensure_action(None, false, false),
        EnsureAction::RequireInvite
    );
}

#[test]
fn claim_url_uses_selected_server_origin() {
    assert_eq!(
        claim_link_url("https://git.example.com", "aa", "c2VjcmV0",).expect("custom HTTPS origin"),
        "https://git.example.com/claim/hcl1.aa.c2VjcmV0"
    );
    assert_eq!(
        claim_link_url("selfhosted.example:8443", "node", "secret").expect("host:port origin"),
        "https://selfhosted.example:8443/claim/hcl1.node.secret"
    );
    assert_eq!(
        claim_link_url("api.heddle.sh", "aa", "c2VjcmV0").expect("default hosted origin"),
        "https://heddle.sh/claim/hcl1.aa.c2VjcmV0"
    );
}

#[test]
fn claim_url_refuses_non_https_and_never_rewrites_custom_hosts_to_heddle() {
    assert!(
        claim_link_url("http://evil.example", "aa", "secret").is_err(),
        "plain HTTP must be refused"
    );
    assert!(
        claim_link_url("https://user@git.example.com", "aa", "secret").is_err(),
        "userinfo must be refused"
    );
    let url = claim_link_url("selfhosted.example", "node", "secret").expect("custom origin");
    assert!(url.starts_with("https://selfhosted.example/claim/hcl1."));
    assert!(
        !url.contains("heddle.sh"),
        "self-hosted claim secrets must not be placed on heddle.sh"
    );
}
