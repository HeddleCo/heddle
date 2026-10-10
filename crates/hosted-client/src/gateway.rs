//! Explicit, separately enrolled native publisher for a hosted Git gateway.
//!
//! This adapter never discovers saved credentials or derives native authority
//! from the Git user's session. GetIdentity observes the configured gateway
//! credential; native admission still checks current Spool write authority.
use std::{
    collections::BTreeSet,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use api::heddle::api::v1alpha2 as wire;
use biscuit_auth::{Biscuit, PublicKey};
use biscuit_verifier::git_transport::GitScope;
use crypto::{Ed25519Signer, Signer};
use heddleco_capability_verifier::{VerifiedOwnerState, thread_control_authority as authority};
use objects::object::{
    StateId,
    thread_replication::{SOURCE_AUTHORIZATION_METHOD, SourceAuthor, ThreadOperation},
};
use prost::Message;
use uuid::Uuid;

use crate::client::{ClientConfig, HostedClient};

/// Native client and original signer built solely from explicit configuration.
/// Deliberately has no Debug implementation because it retains a signing key.
pub struct GatewayClient {
    hosted: HostedClient,
    signer: Ed25519Signer,
    author: SourceAuthor,
    root: [u8; 32],
    credential_blocks: Vec<Vec<u8>>,
}

impl GatewayClient {
    pub async fn connect(
        server: &str,
        config: &ClientConfig,
        author: SourceAuthor,
    ) -> Result<Self> {
        let (signer, root, credential_blocks) = validate_configuration(config, &author)?;
        // server_key activates the user's local netd and credential discovery.
        // Explicit descriptor pins and credentials suffice for a direct client.
        let mut explicit = config.clone();
        explicit.server_key = None;
        let hosted = HostedClient::connect_server(server, &explicit).await?;
        let client = Self {
            hosted,
            signer,
            author,
            root,
            credential_blocks,
        };
        client.current_identity().await?;
        Ok(client)
    }

    pub fn hosted(&self) -> &HostedClient {
        &self.hosted
    }
    pub fn signer(&self) -> &Ed25519Signer {
        &self.signer
    }
    pub fn source_author(&self) -> &SourceAuthor {
        &self.author
    }

    /// Refresh the original gateway author's current credential/owner evidence
    /// before staging or signing. This snapshot is bounded to one exact target
    /// and 30 seconds; it is not a DB write grant or historical closure proof.
    pub async fn refresh_source_authority(
        &self,
        scope: &GitScope,
    ) -> Result<GatewaySourceAuthority> {
        scope.validate()?;
        let SourceAuthor::Account {
            spool,
            actor,
            authority: proof,
            ..
        } = &self.author
        else {
            anyhow::bail!("gateway requires an enrolled Account SourceAuthor");
        };
        ensure!(*spool == scope.spool_id, "gateway author Spool differs");
        let current = self.current_identity().await?;
        let result = GatewaySourceAuthority {
            author: self.author.clone(),
            publisher: self
                .signer
                .public_key()
                .try_into()
                .map_err(|_| anyhow::anyhow!("publisher key length"))?,
            scope: scope.clone(),
            owner: current.owner,
            root: self.root,
            active_identifiers: current.active_identifiers,
            retained: current.retained,
            expires_at: current.expires_at,
            observed: Instant::now(),
        };
        result.verify_proof(proof, actor.principal_id, actor.agent_id.as_deref())?;
        Ok(result)
    }

    /// Fetch and install an exact native State into a caller-created keyless
    /// `Repository::init_clone` repository. The existing native importer checks
    /// the complete authenticated closure and fresh disclosure during commit.
    pub async fn hydrate_exact(
        &self,
        repo: &repo::Repository,
        scope: &GitScope,
        open: wire::FetchOpen,
        limits: thread_api::fetch::Limits,
        scratch: &Path,
    ) -> Result<StateId> {
        scope.validate()?;
        let thread = open.thread.as_ref().context("exact Thread required")?;
        ensure!(
            thread.spool.as_ref().map(|s| s.id.as_str())
                == Some(scope.spool_id.to_string().as_str()),
            "Fetch Spool differs"
        );
        ensure!(
            thread.id.as_ref().map(|id| id.value.as_slice()) == Some(scope.thread_id.as_slice()),
            "Fetch Thread differs"
        );
        let revision = open
            .revision
            .as_ref()
            .context("exact native revision required")?;
        ensure!(
            revision.spool.as_ref() == thread.spool.as_ref(),
            "revision Spool differs"
        );
        let Some(wire::revision_ref::Revision::State(state)) = &revision.revision else {
            anyhow::bail!("gateway hydration requires an exact native State");
        };
        let expected = StateId::try_from_slice(&state.value)?;
        self.refresh_source_authority(scope).await?;
        let staged = self
            .hosted
            .fetch_native_source(repo, open, limits, scratch)
            .await
            .context("gateway exact native source fetch")?;
        ensure!(
            staged.state().id() == expected && staged.is_complete(),
            "incomplete or mismatched native hydration"
        );
        self.refresh_source_authority(scope).await?;
        let installed = self
            .hosted
            .install_staged_source(repo, staged)
            .await
            .context("gateway exact native source installation")?;
        ensure!(installed == expected, "installed State differs");
        Ok(installed)
    }

    async fn current_identity(&self) -> Result<CurrentGatewayIdentity> {
        // This response comes over the configured pinned, authenticated native
        // connection, not an HTTP actor label or an incoming SourceAuthor.
        let response = self.hosted.get_identity().await?;
        validate_current_identity(
            &self.author,
            &self.root,
            &self.credential_blocks,
            &self.signer.public_key(),
            response,
        )
    }
}

/// Short-lived observation for the synchronous `bind_signed` callback. It only
/// covers new operations by this gateway. The caller must independently check
/// the fresh exact retained closure for every historical original author, and
/// the receiver must repeat account/revocation/write gates under its SQL fence.
pub struct GatewaySourceAuthority {
    author: SourceAuthor,
    publisher: [u8; 32],
    scope: GitScope,
    owner: VerifiedOwnerState,
    root: [u8; 32],
    active_identifiers: BTreeSet<String>,
    retained: Vec<wire::SignedOwnerMintRootAttachment>,
    expires_at: i64,
    observed: Instant,
}
impl GatewaySourceAuthority {
    pub fn verify_original(&self, operation: &ThreadOperation) -> Result<()> {
        ensure!(
            operation.publisher == self.publisher,
            "operation publisher is not the gateway proof key"
        );
        ensure!(
            operation.thread.as_bytes() == &self.scope.thread_id,
            "operation Thread differs"
        );
        let author = operation
            .source_author()?
            .context("original source authority required")?;
        ensure!(
            author == self.author,
            "operation SourceAuthor differs from configured gateway author"
        );
        let SourceAuthor::Account {
            actor, authority, ..
        } = &author
        else {
            anyhow::bail!("account source author required");
        };
        self.verify_proof(authority, actor.principal_id, actor.agent_id.as_deref())
    }

    fn verify_proof(&self, proof: &[u8], account: Uuid, agent: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        ensure!(
            self.observed.elapsed() < Duration::from_secs(30) && now < self.expires_at,
            "refresh gateway authority before signing"
        );
        authority::verify_with_retained_mint_roots(
            proof,
            authority::Context {
                owner: &self.owner,
                account_uuid: account.as_bytes(),
                publisher: &self.publisher,
                agent_id: agent,
                method: SOURCE_AUTHORIZATION_METHOD,
                spool_path: &self.scope.repository_path,
                now,
            },
            &self.retained,
            |revocation| match revocation {
                // Only the exact chain/root/key authenticated by fresh GetIdentity
                // is observed active. Unknown selectors fail closed.
                authority::Revocation::Credential(id) => !self.active_identifiers.contains(id),
                authority::Revocation::MintRoot(key) => key != self.root.as_slice(),
                authority::Revocation::Publisher(key) => key != self.publisher.as_slice(),
            },
        )?;
        Ok(())
    }
}

fn envelope(bytes: &[u8]) -> Result<wire::ThreadControlAuthority> {
    ensure!(
        !bytes.is_empty() && bytes.len() <= authority::MAX_BYTES,
        "missing or oversized gateway owner evidence"
    );
    let proof =
        api::mint_root_association::decode_thread_control_authority_for_verification(bytes)?;
    ensure!(
        proof.format == 1
            && proof.encode_to_vec() == bytes
            && proof.mint_root_public_key.len() == 32,
        "noncanonical gateway owner evidence"
    );
    Ok(proof)
}

fn sealed(proof: &wire::ThreadControlAuthority) -> Result<Biscuit> {
    let key = PublicKey::from_bytes(
        &proof.mint_root_public_key,
        biscuit_auth::Algorithm::Ed25519,
    )?;
    let token = biscuit_verifier::signature_v1::verify(&proof.sealed_biscuit, key)?;
    ensure!(
        matches!(token.seal(), Err(biscuit_auth::error::Token::AlreadySealed)),
        "public gateway credential must be sealed"
    );
    Ok(token)
}

fn validate_configuration(
    config: &ClientConfig,
    author: &SourceAuthor,
) -> Result<(Ed25519Signer, [u8; 32], Vec<Vec<u8>>)> {
    ensure!(
        config
            .descriptor_public_key
            .is_some_and(|key| key != [0; 32])
            && config
                .descriptor_key_id
                .as_deref()
                .is_some_and(|id| !id.is_empty()),
        "explicit gateway deployment descriptor pin required"
    );
    ensure!(
        !config.tls_skip_verify && !config.allow_insecure,
        "gateway transport must verify server security"
    );
    ensure!(
        config
            .authenticated_principal
            .as_deref()
            .is_some_and(|id| !id.is_empty()),
        "explicit native signing principal required"
    );
    author.validate()?;
    let SourceAuthor::Account {
        actor, authority, ..
    } = author
    else {
        anyhow::bail!("gateway requires an enrolled Account SourceAuthor");
    };
    let signer = Ed25519Signer::from_pem(
        config
            .auth_proof_key_pem
            .as_deref()
            .context("explicit gateway proof key required")?,
    )?;
    let proof = envelope(authority)?;
    let root = PublicKey::from_bytes(
        &proof.mint_root_public_key,
        biscuit_auth::Algorithm::Ed25519,
    )?;
    let token = biscuit_verifier::parse_token(
        &config
            .token
            .as_ref()
            .context("explicit gateway Biscuit required")?
            .id,
        &[root],
    )?;
    let inspected = biscuit_verifier::inspect_verified_credential(&token, &root)?;
    ensure!(
        inspected.proof_public_key == signer.public_key(),
        "gateway signer is not credential's effective proof key"
    );
    ensure!(
        inspected
            .asserted_account
            .is_none_or(|account| account == actor.principal_id)
            && inspected.agent_id == actor.agent_id,
        "gateway credential and SourceAuthor identity differ"
    );
    let public_token = sealed(&proof)?;
    ensure!(
        public_token.revocation_identifiers() == token.revocation_identifiers(),
        "SourceAuthor does not seal the configured gateway credential chain"
    );
    Ok((
        signer,
        proof
            .mint_root_public_key
            .try_into()
            .map_err(|_| anyhow::anyhow!("mint root length"))?,
        token.revocation_identifiers(),
    ))
}

struct CurrentGatewayIdentity {
    owner: VerifiedOwnerState,
    active_identifiers: BTreeSet<String>,
    retained: Vec<wire::SignedOwnerMintRootAttachment>,
    expires_at: i64,
}
fn validate_current_identity(
    author: &SourceAuthor,
    root: &[u8; 32],
    blocks: &[Vec<u8>],
    publisher: &[u8],
    response: wire::GetIdentityResponse,
) -> Result<CurrentGatewayIdentity> {
    let SourceAuthor::Account {
        actor, authority, ..
    } = author
    else {
        anyhow::bail!("account source author required");
    };
    let identity = response
        .identity
        .context("current gateway account absent")?;
    let current = response
        .current_credential
        .context("current gateway credential absent")?;
    let now = chrono::Utc::now().timestamp();
    let expiry = current
        .expires_at
        .context("current gateway expiry absent")?;
    ensure!(
        !current.revoked
            && (expiry.seconds == 0 || expiry.seconds > now)
            && (0..1_000_000_000).contains(&expiry.nanos),
        "gateway credential expired or revoked"
    );
    ensure!(
        Uuid::parse_str(&identity.account_id)? == actor.principal_id
            && current.proof_public_key == publisher,
        "current gateway account or proof key differs"
    );
    ensure!(
        current.acting_agent_id == actor.agent_id.as_deref().unwrap_or_default()
            && identity.acting_agent_id == current.acting_agent_id,
        "current gateway agent differs"
    );
    let fresh = envelope(&current.thread_control_authority)?;
    let configured = envelope(authority)?;
    ensure!(
        fresh.mint_root_public_key.as_slice() == root.as_slice()
            && fresh.mint_root_association == configured.mint_root_association,
        "current gateway mint association differs"
    );
    let token = sealed(&fresh)?;
    ensure!(
        token.revocation_identifiers().as_slice() == blocks,
        "current credential is not the configured gateway chain"
    );
    let key = PublicKey::from_bytes(root, biscuit_auth::Algorithm::Ed25519)?;
    let inspected = biscuit_verifier::inspect_verified_credential(&token, &key)?;
    ensure!(
        inspected.proof_public_key == publisher
            && inspected.agent_id == actor.agent_id
            && inspected
                .asserted_account
                .is_none_or(|id| id == actor.principal_id),
        "current gateway credential attribution differs"
    );
    let owner = heddleco_capability_verifier::creation::history_state(
        fresh
            .owner
            .as_ref()
            .context("current gateway owner history absent")?,
        now,
    )?;
    ensure!(
        owner
            .signed_root()
            .root
            .as_ref()
            .context("owner root absent")?
            .account_uuid
            == actor.principal_id.as_bytes(),
        "current owner history belongs to another account"
    );
    let original_owner = heddleco_capability_verifier::creation::history_state(
        configured
            .owner
            .as_ref()
            .context("configured gateway owner history absent")?,
        now,
    )?;
    ensure!(
        owner.extends(&original_owner) && owner.owner_id() == original_owner.owner_id(),
        "configured gateway owner history is not a prefix of current account authority"
    );
    let retained = match fresh.mint_root_association {
        Some(wire::thread_control_authority::MintRootAssociation::OwnerMintRootAttachment(
            value,
        )) => vec![value],
        _ => Vec::new(),
    };
    Ok(CurrentGatewayIdentity {
        owner,
        retained,
        // The existing account model uses exp=0 for an unbounded original
        // credential. The preparation observation itself is always bounded.
        expires_at: (if expiry.seconds == 0 {
            now + 30
        } else {
            expiry.seconds
        })
        .min(if inspected.expires_at_unix_seconds == 0 {
            now + 30
        } else {
            i64::try_from(inspected.expires_at_unix_seconds)?
        }),
        active_identifiers: inspected
            .revocation_identities()
            .map(str::to_owned)
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use biscuit_auth::{Algorithm, KeyPair, PrivateKey};
    use biscuit_verifier::{git_transport::GitAction, signature_v1::BiscuitBuilderV1Ext as _};
    use objects::object::{
        CollaborationActor, ContentHash,
        thread_replication::{AuthoredCapture, Capture, ThreadOperationBody},
    };

    fn fixture() -> (
        ClientConfig,
        SourceAuthor,
        wire::GetIdentityResponse,
        GitScope,
    ) {
        let account = Uuid::from_u128(71);
        let signer = Ed25519Signer::from_seed(&[71; 32]).expect("synthetic key");
        let now = chrono::Utc::now();
        let signed = crypto::owner_root::sign_claimable_deferred_human_root(
            &signer,
            *account.as_bytes(),
            [72; 32],
            now.timestamp(),
        )
        .expect("synthetic owner root");
        let owner = heddleco_capability_verifier::verify_owner_root(&signed).expect("owner");
        let history = wire::OwnerHistory {
            root: Some(signed),
            accepted_transitions: vec![],
            state_hash: owner.state_hash().to_vec(),
        };
        let key = KeyPair::from(
            &PrivateKey::from_bytes(&[71; 32], Algorithm::Ed25519).expect("synthetic mint"),
        );
        let expiry = now + chrono::Duration::minutes(10);
        let token = Biscuit::builder().code(format!(
            "user(\"{account}\"); session(\"synthetic-gateway\"); device_pop_key(\"{}\"); expires_at({}); check if time($t), $t < {}; check if resource(\"spool\", \"org/project\"); check if operation(\"PublishContent\") or operation(\"GetIdentity\");",
            hex::encode(signer.public_key()), expiry.to_rfc3339(), expiry.to_rfc3339()).as_str()).expect("caveats").build_v1(&key).expect("token");
        let proof = authority::encode(&history, signer.public_key(), None, &token)
            .expect("sealed owner evidence");
        let scope = GitScope {
            service_audience: "git-gateway".into(),
            tenant_spool_id: Uuid::from_u128(72),
            spool_id: Uuid::from_u128(73),
            repository_path: "org/project".into(),
            thread_id: [74; 32],
            action: GitAction::Write,
            disclosure_audience: "public".into(),
        };
        let author = SourceAuthor::account(
            scope.spool_id,
            CollaborationActor {
                principal_id: account,
                agent_id: None,
            },
            proof.clone(),
        )
        .expect("author");
        let mut config = ClientConfig::new("synthetic-gateway");
        config.token = Some(::wire::AuthToken::new(
            token.to_base64().expect("token bytes"),
            account.to_string(),
        ));
        config.auth_proof_key_pem = Some(signer.to_pem().expect("PEM"));
        config.authenticated_principal = Some(format!("principal:{account}"));
        config.descriptor_key_id = Some("synthetic-pin".into());
        config.descriptor_public_key = Some([75; 32]);
        let response = wire::GetIdentityResponse {
            identity: Some(wire::PrincipalRecord {
                account_id: account.to_string(),
                ..Default::default()
            }),
            current_credential: Some(wire::CurrentCredentialRecord {
                proof_public_key: signer.public_key().to_vec(),
                expires_at: Some(prost_types::Timestamp {
                    seconds: expiry.timestamp(),
                    nanos: 0,
                }),
                thread_control_authority: proof,
                ..Default::default()
            }),
            ..Default::default()
        };
        (config, author, response, scope)
    }

    #[test]
    fn gateway_requires_explicit_pin_and_exact_effective_signing_key() {
        let (mut config, author, _, _) = fixture();
        validate_configuration(&config, &author).expect("genuine synthetic credential");
        config.auth_proof_key_pem = Some(
            Ed25519Signer::from_seed(&[70; 32])
                .expect("other key")
                .to_pem()
                .expect("PEM"),
        );
        assert!(
            validate_configuration(&config, &author).is_err(),
            "gateway cannot splice another signer onto client proof"
        );
        let (mut config, author, _, _) = fixture();
        config.descriptor_public_key = None;
        assert!(
            validate_configuration(&config, &author).is_err(),
            "no ambient descriptor trust"
        );
        let (config, _, _, _) = fixture();
        assert!(
            validate_configuration(&config, &SourceAuthor::LocalKey).is_err(),
            "no unenrolled native publisher"
        );
    }

    #[test]
    fn current_gateway_identity_binds_account_key_chain_and_revocation() {
        let (config, author, response, _) = fixture();
        let (signer, root, blocks) =
            validate_configuration(&config, &author).expect("configuration");
        validate_current_identity(
            &author,
            &root,
            &blocks,
            signer.public_key(),
            response.clone(),
        )
        .expect("current authority");
        for variant in 0..5 {
            let mut changed = response.clone();
            match variant {
                0 => {
                    changed.identity.as_mut().expect("identity").account_id =
                        Uuid::from_u128(99).to_string()
                }
                1 => {
                    changed
                        .current_credential
                        .as_mut()
                        .expect("credential")
                        .revoked = true
                }
                2 => {
                    changed
                        .current_credential
                        .as_mut()
                        .expect("credential")
                        .proof_public_key = vec![1; 32]
                }
                3 => {
                    changed
                        .current_credential
                        .as_mut()
                        .expect("credential")
                        .expires_at
                        .as_mut()
                        .expect("expiry")
                        .seconds = 1
                }
                _ => changed
                    .current_credential
                    .as_mut()
                    .expect("credential")
                    .thread_control_authority
                    .clear(),
            }
            assert!(
                validate_current_identity(&author, &root, &blocks, signer.public_key(), changed)
                    .is_err(),
                "changed authority variant {variant}"
            );
        }
        assert!(
            validate_current_identity(
                &author,
                &root,
                &[vec![1; 64]],
                signer.public_key(),
                response
            )
            .is_err(),
            "same actor/key does not prove the same credential chain"
        );
    }

    #[test]
    fn current_gateway_accepts_the_scoped_delegated_leaf_not_its_root_signer() {
        let (mut config, author, mut response, scope) = fixture();
        let parent = Ed25519Signer::from_seed(&[71; 32]).expect("root signer");
        let leaf = Ed25519Signer::from_seed(&[77; 32]).expect("scoped leaf signer");
        let token = &config.token.as_ref().expect("token").id;
        let leaf_key: [u8; 32] = leaf.public_key().try_into().expect("leaf key");
        let statement =
            biscuit_verifier::key_delegation::statement(token, &leaf_key).expect("transition");
        let delegated = biscuit_verifier::key_delegation::append(
            token,
            &leaf_key,
            &parent
                .sign(&statement)
                .expect("root signed delegation")
                .try_into()
                .expect("signature length"),
            biscuit_auth::builder::BlockBuilder::new()
                .check("check if resource(\"spool\", \"org/project\")")
                .expect("scoped restriction"),
        )
        .expect("append child");
        let SourceAuthor::Account {
            actor,
            authority: proof,
            ..
        } = author
        else {
            panic!("account");
        };
        let mut proof = envelope(&proof).expect("proof");
        let key =
            PublicKey::from_bytes(&proof.mint_root_public_key, Algorithm::Ed25519).expect("mint");
        let token = biscuit_verifier::parse_token(&delegated, &[key]).expect("delegated token");
        proof.sealed_biscuit = token
            .seal()
            .expect("sealed leaf")
            .to_vec()
            .expect("leaf bytes");
        let proof = proof.encode_to_vec();
        let author =
            SourceAuthor::account(scope.spool_id, actor, proof.clone()).expect("leaf author");
        config.token.as_mut().expect("token").id = delegated;
        config.auth_proof_key_pem = Some(leaf.to_pem().expect("leaf PEM"));
        let current = response.current_credential.as_mut().expect("credential");
        current.proof_public_key = leaf.public_key().to_vec();
        current.thread_control_authority = proof;
        let (signer, root, blocks) =
            validate_configuration(&config, &author).expect("effective leaf config");
        let current =
            validate_current_identity(&author, &root, &blocks, signer.public_key(), response)
                .expect("current delegated chain");
        let snapshot = GatewaySourceAuthority {
            author: author.clone(),
            publisher: leaf.public_key().try_into().expect("leaf key"),
            scope: scope.clone(),
            owner: current.owner,
            root,
            active_identifiers: current.active_identifiers,
            retained: current.retained,
            expires_at: current.expires_at,
            observed: Instant::now(),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::from_bytes(scope.thread_id),
            parents: Default::default(),
            publisher: leaf.public_key().try_into().expect("leaf key"),
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result: Capture::from(vec![]),
                author: author.clone(),
            }),
        };
        snapshot
            .verify_original(&operation)
            .expect("native author verified with effective leaf");
        config.auth_proof_key_pem = Some(parent.to_pem().expect("root PEM"));
        assert!(
            validate_configuration(&config, &author).is_err(),
            "ancestor key cannot replace effective leaf"
        );
    }

    #[test]
    fn signing_snapshot_is_exact_scoped_and_cannot_authorize_old_authors() {
        let (config, author, response, scope) = fixture();
        let (signer, root, blocks) =
            validate_configuration(&config, &author).expect("configuration");
        let current =
            validate_current_identity(&author, &root, &blocks, signer.public_key(), response)
                .expect("identity");
        let mut snapshot = GatewaySourceAuthority {
            author: author.clone(),
            publisher: signer.public_key().try_into().expect("key"),
            scope: scope.clone(),
            owner: current.owner,
            root,
            active_identifiers: current.active_identifiers,
            retained: current.retained,
            expires_at: current.expires_at,
            observed: Instant::now(),
        };
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::from_bytes(scope.thread_id),
            parents: Default::default(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(AuthoredCapture {
                result: Capture::from(vec![]),
                author,
            }),
        };
        snapshot
            .verify_original(&operation)
            .expect("exact new gateway operation");
        let mut other = operation.clone();
        other.publisher = [99; 32];
        assert!(snapshot.verify_original(&other).is_err());
        other = operation.clone();
        other.thread = ContentHash::from_bytes([99; 32]);
        assert!(snapshot.verify_original(&other).is_err());
        snapshot.scope.repository_path = "org/other".into();
        assert!(
            snapshot.verify_original(&operation).is_err(),
            "PublishContent resource caveat still applies"
        );
        snapshot.scope = scope;
        snapshot.observed = Instant::now() - Duration::from_secs(31);
        assert!(
            snapshot.verify_original(&operation).is_err(),
            "snapshot must be refreshed"
        );
    }
}
