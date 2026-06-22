//! Typed requests for `heddle auth` handlers.

#[derive(Clone, Debug)]
pub enum AuthCommand {
    Login {
        server: String,
        no_browser: bool,
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
    },
}
