//! Typed requests for `heddle auth` handlers.

#[derive(Clone, Debug)]
pub enum AuthCommand {
    Login {
        server: Option<String>,
        open_browser: bool,
        token: Option<String>,
        key_file: Option<std::path::PathBuf>,
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
        out: Option<std::path::PathBuf>,
        stdout: bool,
    },
    CreateServiceToken {
        name: String,
        namespace: String,
        server: Option<String>,
        /// Optional path for the private-key PEM (default: under heddle home).
        key_out: Option<String>,
        /// Include private key material in stdout / JSON.
        show_secrets: bool,
    },
}
