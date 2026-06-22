//! Typed requests for `heddle support` handlers.

#[derive(Clone, Debug)]
pub enum SupportCommand {
    Grant(SupportGrant),
    List(SupportList),
    Revoke(SupportRevoke),
}

#[derive(Clone, Debug)]
pub struct SupportGrant {
    pub operator_email: String,
    pub namespace: Option<String>,
    pub repo: Option<String>,
    pub ttl: String,
    pub reason: String,
    pub remote: String,
}

#[derive(Clone, Debug)]
pub struct SupportList {
    pub namespace: Option<String>,
    pub repo: Option<String>,
    pub include_inactive: bool,
    pub remote: String,
}

#[derive(Clone, Debug)]
pub struct SupportRevoke {
    pub id: String,
    pub remote: String,
}
