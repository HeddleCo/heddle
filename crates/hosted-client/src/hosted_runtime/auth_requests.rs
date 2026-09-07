//! Typed requests for hosted authentication operations.

/// Per-operation metadata supplied by an embedding caller.
#[derive(Clone, Debug, Default)]
pub struct AuthOptions {
    operation_id: Option<String>,
}

impl AuthOptions {
    pub fn new(operation_id: Option<String>) -> Self {
        Self {
            operation_id: operation_id.filter(|value| !value.is_empty()),
        }
    }

    pub(crate) fn operation_id(&self) -> Option<&str> {
        self.operation_id.as_deref()
    }
}

/// Whether login may wait for a human browser ceremony.
///
/// A process Adapter such as the CLI decides whether a terminal is available;
/// the hosted Module only receives this semantic permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginPermission {
    HeadlessOnly,
    Browser { open_browser: bool },
}

#[derive(Clone, Debug)]
pub enum AuthCommand {
    Login {
        server: Option<String>,
        permission: LoginPermission,
        /// Invite consumed only when this machine has no hosted account yet.
        invite: Option<String>,
        /// Install a verified `.hcred` credential file without a browser.
        /// The server comes from the file. Mutually exclusive with the
        /// browser flags.
        credential: Option<std::path::PathBuf>,
    },
    Logout {
        server: Option<String>,
    },
    Status {
        server: Option<String>,
    },
    Invite {
        email: Option<String>,
        server: Option<String>,
        list: bool,
    },
    Trust {
        command: AuthTrustCommand,
    },
    DeriveAgent {
        server: String,
        agent_id: Option<String>,
        ttl_secs: u64,
        scopes: Vec<String>,
        allowed_operations: Vec<String>,
        /// Preset operation ceiling (`reviewer` | `contributor` | `ci-landing`
        /// or the internal `--runner` persona).
        /// Expands to a curated `--allow` set; a combined explicit `--allow`
        /// may only narrow it.
        template: Option<crate::hosted_runtime::device_flow::AgentTemplate>,
        /// Write a single `<name>.hcred` credential file to this path instead
        /// of installing the child into the keystore.
        out: Option<std::path::PathBuf>,
    },
    CreateServiceToken {
        name: String,
        namespace: String,
        server: Option<String>,
        /// Path for the `.hcred` credential file
        /// (default: `~/.heddle/service-accounts/<name>.hcred`).
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Clone, Debug)]
pub enum AuthTrustCommand {
    Show {
        server: String,
    },
    Replace {
        server: String,
        expected_current_public_key: String,
        key_id: String,
        public_key: String,
    },
}
