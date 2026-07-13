//! Typed requests for `heddle auth` handlers.

#[derive(Clone, Debug)]
pub enum AuthCommand {
    Login {
        server: String,
        open_browser: bool,
    },
    Logout {
        server: Option<String>,
    },
    Status {
        server: Option<String>,
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
