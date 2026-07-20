//! Typed requests for `heddle auth` handlers.

#[derive(Clone, Debug)]
pub enum AuthCommand {
    Login {
        server: Option<String>,
        open_browser: bool,
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
    DeriveAgent {
        server: String,
        agent_id: Option<String>,
        ttl_secs: u64,
        scopes: Vec<String>,
        allowed_operations: Vec<String>,
        /// Preset operation ceiling (`reviewer` | `contributor` | `ci-landing`).
        /// Expands to a curated `--allow` set; a combined explicit `--allow`
        /// may only narrow it.
        template: Option<crate::device_flow::AgentTemplate>,
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
