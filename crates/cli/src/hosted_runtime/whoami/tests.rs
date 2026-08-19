use cli_shared::{ResolvedPrincipal, UserConfig, resolve_principal_without_repo};
use objects::object::Principal;

use super::hosted::{biscuit_string_literals, hosted_role_name, resolve_local_whoami};
use super::report::{CaptureActor, WhoamiIdentity, WhoamiOutput, WhoamiRole, write_human};
use crate::hosted_runtime::hosted::{CredentialSource, ResolvedHostedCredential};

fn luke_actor() -> CaptureActor {
    CaptureActor::from_resolved(&ResolvedPrincipal {
        principal: Principal::new("Luke", "luke@example.com"),
        source: Some("user_config"),
    })
}

#[test]
fn capture_actor_from_resolved_maps_user_config() {
    let actor = luke_actor();
    assert_eq!(actor.name, "Luke");
    assert_eq!(actor.email, "luke@example.com");
    assert_eq!(actor.source, Some("user_config"));
}

#[test]
fn without_repo_uses_user_config_when_env_unset() {
    let _guard = PrincipalEnvGuard::clear();
    let user_config = UserConfig {
        principal: Some(cli_shared::config::UserPrincipalConfig {
            name: "Luke".to_string(),
            email: "luke@example.com".to_string(),
        }),
        ..UserConfig::default()
    };
    let resolved = resolve_principal_without_repo(&user_config);
    assert_eq!(resolved.source, Some("user_config"));
    assert_eq!(resolved.principal.name_lossy(), "Luke");
    assert_eq!(resolved.principal.email_lossy(), "luke@example.com");
}

#[test]
fn local_unauthenticated_identity_has_actionable_output() {
    let resolved = ResolvedHostedCredential {
        token: None,
        proof_key_pem: None,
        renewable: None,
        subject: None,
        credential_id: None,
        expires_at: None,
        source: CredentialSource::Unauthenticated,
    };
    let output = resolve_local_whoami("host.example", &resolved, luke_actor()).unwrap();
    assert!(!output.authenticated);
    assert_eq!(output.source, "none");
    assert_eq!(output.capture_actor.name, "Luke");
    assert_eq!(output.capture_actor.source, Some("user_config"));
    assert_eq!(
        output.recommended_action.as_deref(),
        Some("heddle auth login --server host.example")
    );
    let mut rendered = Vec::new();
    write_human(&mut rendered, &output).unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(
        rendered.contains("Capture actor: Luke <luke@example.com>"),
        "capture actor must appear:\n{rendered}"
    );
    let actor_at = rendered
        .find("Capture actor:")
        .expect("capture actor stanza");
    let hosted_at = rendered.find("Hosted auth:").expect("hosted auth stanza");
    assert!(
        actor_at < hosted_at,
        "capture actor must come before hosted auth:\n{rendered}"
    );
    assert!(rendered.contains("Not authenticated with host.example."));
    assert_eq!(
        rendered,
        "Capture actor: Luke <luke@example.com>\n\
         Source:        user_config (shared global config)\n\
         \n\
         Hosted auth:\n\
         Server:        host.example\n\
         Not authenticated with host.example.\n\
         Run `heddle auth login --server host.example` to authenticate.\n"
    );
}

#[test]
fn human_output_renders_authoritative_identity_and_local_token_facts() {
    let mut output = WhoamiOutput {
        output_kind: "whoami",
        capture_actor: luke_actor(),
        server: "host.example".to_string(),
        authenticated: true,
        source: "keystore".to_string(),
        subject: Some("principal:alice".to_string()),
        reachable: true,
        token_kind: Some("agent".to_string()),
        scopes: vec!["repo:alice/widgets".to_string()],
        operation_ceiling: Some(vec!["repo.read".to_string(), "repo.push".to_string()]),
        expires_at: Some("2030-01-01T00:00:00Z".to_string()),
        ttl_seconds_remaining: Some(60),
        proof_key_available: true,
        identity: Some(WhoamiIdentity {
            subject: "principal:alice".to_string(),
            actor_subject: "agent:reviewer".to_string(),
            is_staff: true,
            is_service_account: false,
            is_biscuit: true,
            session_id: "session-1".to_string(),
            amr: vec!["device".to_string()],
            server_scope: "hosted".to_string(),
            credential_id: "credential-1".to_string(),
            device_id: Some("device-1".to_string()),
            agent_provider: Some("openai".to_string()),
            agent_model: Some("codex".to_string()),
            roles: vec![WhoamiRole {
                resource_path: "alice/widgets".to_string(),
                resource_kind: "repo".to_string(),
                role: "maintainer".to_string(),
            }],
        }),
        recommended_action: None,
    };
    let mut rendered = Vec::new();
    write_human(&mut rendered, &output).unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains("Capture actor: Luke <luke@example.com>"));
    assert!(rendered.contains("Hosted auth:"));
    assert!(rendered.contains("Acting as:     agent:reviewer"));
    assert!(rendered.contains("Roles:         repo:alice/widgets=maintainer"));
    assert!(rendered.contains("Expires:       2030-01-01T00:00:00Z (in 60s)"));
    output.identity = None;
    output.reachable = false;
    output.scopes.clear();
    output.operation_ceiling = None;
    output.ttl_seconds_remaining = Some(-5);
    output.proof_key_available = false;
    output.recommended_action = Some("heddle auth login --server host.example".to_string());
    let mut rendered = Vec::new();
    write_human(&mut rendered, &output).unwrap();
    let rendered = String::from_utf8(rendered).unwrap();
    assert!(rendered.contains("Server:        unreachable"));
    assert!(rendered.contains("Scopes:        full resource authority"));
    assert!(rendered.contains("EXPIRED 5s ago"));
    output.ttl_seconds_remaining = None;
    let mut rendered = Vec::new();
    write_human(&mut rendered, &output).unwrap();
    assert!(
        String::from_utf8(rendered)
            .unwrap()
            .contains("Expires:       2030-01-01T00:00:00Z\n")
    );
}

#[test]
fn biscuit_literal_and_role_helpers_cover_escaped_and_unknown_values() {
    assert_eq!(
        biscuit_string_literals(r#"check if operation($op), $op == "repo.read""#),
        vec!["repo.read".to_string()]
    );
    assert_eq!(hosted_role_name(1), "reader");
    assert_eq!(hosted_role_name(2), "developer");
    assert_eq!(hosted_role_name(3), "maintainer");
    assert_eq!(hosted_role_name(4), "admin");
    assert_eq!(hosted_role_name(5), "owner");
    assert_eq!(hosted_role_name(i32::MAX), "unspecified");
}

struct PrincipalEnvGuard {
    name: Option<std::ffi::OsString>,
    email: Option<std::ffi::OsString>,
}

impl PrincipalEnvGuard {
    fn clear() -> Self {
        let name = std::env::var_os("HEDDLE_PRINCIPAL_NAME");
        let email = std::env::var_os("HEDDLE_PRINCIPAL_EMAIL");
        unsafe {
            std::env::remove_var("HEDDLE_PRINCIPAL_NAME");
            std::env::remove_var("HEDDLE_PRINCIPAL_EMAIL");
        }
        Self { name, email }
    }
}

impl Drop for PrincipalEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.name {
                Some(value) => std::env::set_var("HEDDLE_PRINCIPAL_NAME", value),
                None => std::env::remove_var("HEDDLE_PRINCIPAL_NAME"),
            }
            match &self.email {
                Some(value) => std::env::set_var("HEDDLE_PRINCIPAL_EMAIL", value),
                None => std::env::remove_var("HEDDLE_PRINCIPAL_EMAIL"),
            }
        }
    }
}
