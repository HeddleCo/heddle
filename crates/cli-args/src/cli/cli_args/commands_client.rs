// SPDX-License-Identifier: Apache-2.0
//! Hosted-client command arguments.

use clap::{Args, Subcommand, ValueEnum};

const DEFAULT_CLAIM_TIMEOUT: &str = "15m";
pub const DEFAULT_CLAIM_WEB_ORIGIN: &str = "https://app.heddle.sh";

/// Arguments for `heddle promote`.
#[derive(Args, Clone, Debug)]
#[command(after_help = "\
Promote moves a personal hosted spool to the shared root namespace:

  spool/<your-handle>/<name>  →  spool/<name>

The server requires the root slug to be free, a claimed/verified account, and
an owner grant on the personal spool. Denials print the recovery step.

Examples:
  heddle promote spool/willow-ibis-8e7264/notes
  heddle promote willow-ibis-8e7264/notes --server api.preview.heddle.sh
  heddle promote https://api.preview.heddle.sh/willow-ibis-8e7264/notes
")]
pub struct PromoteArgs {
    /// Personal spool to lift to root (`spool/<handle>/<name>`, `<handle>/<name>`, or a hosted URL).
    #[arg(value_name = "PATH")]
    pub path: String,

    /// Hosted Heddle server. Omit when `PATH` is a URL, or to use the configured default.
    #[arg(long)]
    pub server: Option<String>,
}

/// Offer the current agent-rooted account for a human to claim.
#[derive(Args, Clone, Debug)]
pub struct ClaimArgs {
    /// Hosted Heddle server. Omit to use the configured default
    /// (`api.heddle.sh` when none is stored).
    #[arg(long)]
    pub server: Option<String>,

    /// HTTPS origin used to build the human claim link. Highest precedence;
    /// must be https (scheme, host, optional port). When omitted, a
    /// server-advertised origin is used only if it shares the hosted
    /// server's registrable domain. `https://app.heddle.sh` is the fallback
    /// only for `api.heddle.sh`.
    #[arg(long, value_name = "ORIGIN")]
    pub web_origin: Option<String>,

    /// How long to keep the claim listener resident (`s`, `m`, `h`, or `d`).
    #[arg(
        long,
        default_value = DEFAULT_CLAIM_TIMEOUT,
        value_name = "DURATION",
        value_parser = parse_claim_timeout
    )]
    pub timeout: std::time::Duration,
}

fn parse_claim_timeout(value: &str) -> Result<std::time::Duration, String> {
    let value = value.trim();
    let (amount, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 60 * 60),
        Some(b'd') => (&value[..value.len() - 1], 24 * 60 * 60),
        Some(byte) if byte.is_ascii_digit() => (value, 1),
        _ => {
            return Err("expected a positive duration such as 30s, 15m, 2h, or 1d".to_string());
        }
    };
    let amount = amount
        .parse::<u64>()
        .map_err(|_| "claim timeout must be a positive whole number".to_string())?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "claim timeout is too large".to_string())?;
    if seconds == 0 {
        return Err("claim timeout must be greater than zero".to_string());
    }
    Ok(std::time::Duration::from_secs(seconds))
}

/// Preset operation ceilings for `heddle auth derive-agent`.
///
/// Each variant expands to a curated set of safe agent operations. `reviewer`
/// and `ci-landing` are strict subsets of the safe ceiling; `contributor` is
/// the full safe ceiling (the named form of the default `--allow`-less
/// derivation). `--scope`/`--allow` stay usable alongside a template and, when
/// combined, may only *narrow* it (they intersect the template's set).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentTemplateArg {
    /// Read + review: every read RPC plus Pull. No writes, no ref moves.
    /// (ListStates/GetState/GetBlame/GetTree/GetBlob/GetDiff/GetCompare/
    /// ListActions/ListContext/GetContextHistory/GetDiscussion/ListByState/
    /// ListBySymbol/... + Pull + WhoAmI.)
    Reviewer,
    /// Read + collaboration writes: the reviewer set plus Push, UpdateRef,
    /// SetContext/ReviseContext/SupersedeContext, and
    /// OpenDiscussion/AppendTurn/ResolveDiscussion. No repo/namespace admin.
    /// This is the full safe agent ceiling — the named form of deriving with
    /// no --template/--allow.
    Contributor,
    /// Read + Pull + the Push/UpdateRef a CI lander needs to run ready/land.
    /// No context or discussion writes.
    #[value(name = "ci-landing")]
    CiLanding,
}

#[derive(Subcommand, Clone, Debug)]
pub enum AuthCommands {
    /// Authenticate with a Heddle server.
    ///
    /// Reuses a valid stored credential, remints a registered node-key
    /// account, consumes `--invite` to create one, or opens the browser
    /// on a TTY. Non-interactive sessions without an account fail closed.
    Login {
        /// Heddle server address. Omit to use the configured default
        /// (`api.heddle.sh` when none is stored).
        #[arg(long)]
        server: Option<String>,

        /// Open the authorization URL in the system browser.
        #[arg(long)]
        open_browser: bool,

        /// Invite consumed only when this machine has no hosted account yet.
        #[arg(long, conflicts_with = "credential")]
        invite: Option<String>,

        /// Install a verified `.hcred` credential file without a browser.
        /// The server is taken from the file.
        #[arg(long, value_name = "HCRED_PATH", conflicts_with_all = ["server", "open_browser", "invite"])]
        credential: Option<std::path::PathBuf>,
    },

    /// Remove stored credentials for a server
    Logout {
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
    },

    /// Show current authentication status
    Status {
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
    },

    /// Create or list signup invites owned by the signed-in account.
    ///
    /// Signup-only: this mints an account-creation code. It does not grant
    /// another principal access to a spool. Use `heddle grant` to add a
    /// collaborator.
    #[command(args_conflicts_with_subcommands = true)]
    #[command(after_help = "\
Signup-only. `heddle auth invite` creates an account-creation code.
It does not grant spool access. Add a collaborator with:

  heddle grant create --spool <path|url> --principal <handle> --role contributor
")]
    Invite {
        /// Bind the new invite to an email address.
        #[arg(long)]
        email: Option<String>,

        /// Heddle server address. Omit to use the configured default
        /// (`api.heddle.sh` when none is stored).
        #[arg(long, global = true)]
        server: Option<String>,

        #[command(subcommand)]
        command: Option<AuthInviteCommands>,
    },

    /// Inspect or explicitly replace the deployment descriptor root pin
    Trust {
        #[command(subcommand)]
        command: AuthTrustCommands,
    },

    /// Derive a scoped, short-lived agent token offline.
    /// Advanced: not a first-screen noun.
    DeriveAgent {
        /// Server whose stored credential is the parent.
        #[arg(long)]
        server: String,

        /// Delegation name recorded in the Biscuit chain.
        #[arg(long)]
        agent_id: Option<String>,

        /// Child lifetime in seconds (clamped by the parent expiry).
        #[arg(long = "ttl", default_value_t = 3600)]
        ttl_secs: u64,

        /// Resource scope (`spool:name`, `spool:handle/name`, or a bare spool path).
        #[arg(long = "scope")]
        scopes: Vec<String>,

        /// Narrow the safe operation set (repeatable, using hosted operation names such as `Push`).
        #[arg(long = "allow")]
        allowed_operations: Vec<String>,

        /// Preset operation ceiling. `reviewer` = read-only + Pull;
        /// `contributor` = reviewer + Push/UpdateRef + context/discussion
        /// writes; `ci-landing` = reviewer + Push/UpdateRef for ready/land.
        /// A combined `--allow` may only narrow the template.
        #[arg(long, value_enum)]
        template: Option<AgentTemplateArg>,

        /// Derive a CI-verdict runner: `ci-verdict:write` on the required
        /// `spool:` scope(s), without Push, UpdateRef, or other source writes.
        #[arg(long, conflicts_with_all = ["template", "allowed_operations"])]
        runner: bool,

        /// Write a single self-verifying `<name>.hcred` credential file to this
        /// path instead of installing the child into the keystore.
        #[arg(long, value_name = "HCRED_PATH")]
        out: Option<std::path::PathBuf>,
    },

    /// Create a service token for CI/scripts, scoped to a namespace.
    /// Advanced: not a first-screen noun.
    CreateServiceToken {
        /// Display name for the service account (e.g. "github-ci-main")
        name: String,
        /// Namespace to scope the token to (e.g. "heddle/platform")
        #[arg(long)]
        namespace: String,
        /// Heddle server address
        #[arg(long)]
        server: Option<String>,
        /// Write the `.hcred` credential file to this path
        /// (default: ~/.heddle/service-accounts/<name>.hcred)
        #[arg(long, value_name = "HCRED_PATH")]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Clone, Debug)]
pub enum AuthInviteCommands {
    /// List signup invites owned by the signed-in account.
    List,
}

/// Hosted role granted on a spool.
///
/// `contributor` is the everyday name for the `developer` role.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantRoleArg {
    Reader,
    Contributor,
    Developer,
    Maintainer,
    Admin,
    Owner,
}

impl GrantRoleArg {
    /// Wire token `CreateGrant` / `UpdateGrant` accept.
    pub fn as_hosted_role_name(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Contributor | Self::Developer => "developer",
            Self::Maintainer => "maintainer",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// Hosted role ordinal used for the agent grant ceiling.
    pub fn as_grant_role(self) -> repo::GrantRole {
        match self {
            Self::Reader => repo::GrantRole::Reader,
            Self::Contributor | Self::Developer => repo::GrantRole::Developer,
            Self::Maintainer => repo::GrantRole::Maintainer,
            Self::Admin => repo::GrantRole::Admin,
            Self::Owner => repo::GrantRole::Owner,
        }
    }
}

/// Grant another principal access to a hosted spool.
///
/// Separate from `heddle auth invite`, which is signup-only.
#[derive(Subcommand, Clone, Debug)]
pub enum GrantCommands {
    /// Grant a principal a role on a hosted spool.
    #[command(after_help = "\
Adds a collaborator to an existing hosted spool. This is not a signup
invite — `heddle auth invite` only creates an account-creation code.

Roles: reader, contributor (developer), maintainer, admin, owner.

Agent sessions may grant writer or below (reader, contributor,
maintainer) without human verification. Admin and owner stay
human-verified and are refused for derive-agent / attenuated sessions.

Examples:
  heddle grant create --spool spool/willow-ibis-8e7264/notes --principal alice --role contributor
  heddle grant create --spool https://api.preview.heddle.sh/notes --principal alice --role reader
")]
    Create(GrantCreateArgs),

    /// List grants on a hosted spool.
    #[command(after_help = "\
Examples:
  heddle grant list --spool spool/willow-ibis-8e7264/notes
  heddle grant list --spool notes --server api.preview.heddle.sh
")]
    List(GrantListArgs),

    /// Remove a grant. `ID` is the principal shown by `heddle grant list`.
    #[command(after_help = "\
Agents may delete writer-or-below grants without human verification.
Deleting an admin or owner grant still requires a human-verified session.

Examples:
  heddle grant delete alice --spool spool/willow-ibis-8e7264/notes
")]
    Delete(GrantDeleteArgs),
}

/// Arguments for `heddle grant create`.
#[derive(Args, Clone, Debug)]
pub struct GrantCreateArgs {
    /// Spool to grant on (`spool/<handle>/<name>`, `<handle>/<name>`, or a hosted URL).
    #[arg(long, value_name = "PATH|URL")]
    pub spool: String,

    /// Principal to grant (handle or account).
    #[arg(long, value_name = "HANDLE|ACCOUNT")]
    pub principal: String,

    /// Role to grant.
    #[arg(long, value_enum)]
    pub role: GrantRoleArg,

    /// Hosted Heddle server. Omit when `--spool` is a URL, or to use the configured default.
    #[arg(long)]
    pub server: Option<String>,
}

/// Arguments for `heddle grant list`.
#[derive(Args, Clone, Debug)]
pub struct GrantListArgs {
    /// Spool whose grants to list (`spool/<handle>/<name>`, `<handle>/<name>`, or a hosted URL).
    #[arg(long, value_name = "PATH|URL")]
    pub spool: String,

    /// Hosted Heddle server. Omit when `--spool` is a URL, or to use the configured default.
    #[arg(long)]
    pub server: Option<String>,
}

/// Arguments for `heddle grant delete`.
#[derive(Args, Clone, Debug)]
pub struct GrantDeleteArgs {
    /// Principal shown as `ID` by `heddle grant list`.
    #[arg(value_name = "ID")]
    pub id: String,

    /// Spool the grant is on (`spool/<handle>/<name>`, `<handle>/<name>`, or a hosted URL).
    #[arg(long, value_name = "PATH|URL")]
    pub spool: String,

    /// Hosted Heddle server. Omit when `--spool` is a URL, or to use the configured default.
    #[arg(long)]
    pub server: Option<String>,
}

#[derive(Subcommand, Clone, Debug)]
pub enum AuthTrustCommands {
    /// Show the descriptor root pin controlling a server connection
    Show(AuthTrustShowArgs),
    /// Atomically replace an automatic descriptor root pin
    Replace(AuthTrustReplaceArgs),
}

#[derive(Args, Clone, Debug)]
pub struct AuthTrustShowArgs {
    /// Heddle server authority
    #[arg(long)]
    pub server: String,
}

#[derive(Args, Clone, Debug)]
pub struct AuthTrustReplaceArgs {
    /// Heddle server authority
    #[arg(long)]
    pub server: String,
    /// Current descriptor root public key required for compare-and-swap
    #[arg(long, value_name = "64_HEX")]
    pub expect_current_public_key: String,
    /// New descriptor root key id confirmed out of band
    #[arg(long)]
    pub key_id: String,
    /// New descriptor root public key confirmed out of band
    #[arg(long, value_name = "64_HEX")]
    pub public_key: String,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use crate::cli::{
        AuthCommands, AuthInviteCommands, AuthTrustCommands, Cli, Commands, GrantCommands,
        GrantRoleArg,
    };

    #[test]
    fn trust_replace_parses_compare_and_swap_inputs() {
        let old_key = "11".repeat(32);
        let new_key = "22".repeat(32);
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "trust",
            "replace",
            "--server",
            "api.example",
            "--expect-current-public-key",
            &old_key,
            "--key-id",
            "next-key",
            "--public-key",
            &new_key,
        ])
        .expect("trust replacement parses");

        let Commands::Auth {
            command:
                AuthCommands::Trust {
                    command: AuthTrustCommands::Replace(args),
                },
        } = cli.command
        else {
            panic!("expected auth trust replace");
        };
        assert_eq!(args.server, "api.example");
        assert_eq!(args.expect_current_public_key, old_key);
        assert_eq!(args.key_id, "next-key");
        assert_eq!(args.public_key, new_key);
    }

    #[test]
    fn login_parses_credential_path() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "login",
            "--credential",
            "/run/secrets/agent.hcred",
        ])
        .expect("credential login flag parses");

        let Commands::Auth {
            command:
                AuthCommands::Login {
                    server,
                    credential,
                    open_browser,
                    invite,
                },
        } = cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(server, None, "server comes from the credential file");
        assert_eq!(
            credential.as_deref(),
            Some(std::path::Path::new("/run/secrets/agent.hcred"))
        );
        assert!(!open_browser);
        assert_eq!(invite, None);
    }

    #[test]
    fn login_credential_conflicts_with_browser_flags() {
        for conflicting in [
            vec![
                "--credential",
                "/run/secrets/agent.hcred",
                "--server",
                "api.heddle.sh",
            ],
            vec!["--credential", "/run/secrets/agent.hcred", "--open-browser"],
            vec![
                "--credential",
                "/run/secrets/agent.hcred",
                "--invite",
                "code",
            ],
        ] {
            let mut args = vec!["heddle", "auth", "login"];
            args.extend(conflicting);
            assert!(
                Cli::try_parse_from(args).is_err(),
                "--credential must not combine with the browser-login flags"
            );
        }
    }

    #[test]
    fn interactive_login_needs_no_flags() {
        Cli::try_parse_from(["heddle", "auth", "login"])
            .expect("interactive login may resolve the configured default server");
    }

    #[test]
    fn claim_is_one_top_level_resident_command() {
        let cli = Cli::try_parse_from([
            "heddle",
            "claim",
            "--server",
            "weft.example",
            "--web-origin",
            "https://heddle.example",
            "--timeout",
            "30m",
        ])
        .expect("claim flags parse");
        let Commands::Claim(args) = cli.command else {
            panic!("expected top-level claim");
        };
        assert_eq!(args.server.as_deref(), Some("weft.example"));
        assert_eq!(args.web_origin.as_deref(), Some("https://heddle.example"));
        assert_eq!(args.timeout, std::time::Duration::from_secs(30 * 60));
        assert!(Cli::try_parse_from(["heddle", "claim", "--timeout", "0s"]).is_err());
    }

    #[test]
    fn promote_parses_path_and_optional_server() {
        let cli = Cli::try_parse_from([
            "heddle",
            "promote",
            "spool/willow-ibis-8e7264/notes",
            "--server",
            "api.preview.heddle.sh",
        ])
        .expect("promote flags parse");
        let Commands::Promote(args) = cli.command else {
            panic!("expected top-level promote");
        };
        assert_eq!(args.path, "spool/willow-ibis-8e7264/notes");
        assert_eq!(args.server.as_deref(), Some("api.preview.heddle.sh"));
        assert!(Cli::try_parse_from(["heddle", "promote"]).is_err());
    }

    #[test]
    fn login_parses_an_optional_invite() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "login",
            "--server",
            "api.heddle.test",
            "--invite",
            "invite-secret",
        ])
        .expect("auth login --invite parses");
        let Commands::Auth {
            command:
                AuthCommands::Login {
                    server,
                    invite,
                    credential,
                    open_browser,
                },
        } = cli.command
        else {
            panic!("expected auth login");
        };
        assert_eq!(server.as_deref(), Some("api.heddle.test"));
        assert_eq!(invite.as_deref(), Some("invite-secret"));
        assert_eq!(credential, None);
        assert!(!open_browser);
    }

    #[test]
    fn signup_invite_create_and_list_parse() {
        let create = Cli::try_parse_from([
            "heddle",
            "auth",
            "invite",
            "--server",
            "api.heddle.test",
            "--email",
            "alice@example.com",
        ])
        .expect("auth invite create flags parse");
        let Commands::Auth {
            command:
                AuthCommands::Invite {
                    email,
                    server,
                    command,
                },
        } = create.command
        else {
            panic!("expected auth invite");
        };
        assert_eq!(email.as_deref(), Some("alice@example.com"));
        assert_eq!(server.as_deref(), Some("api.heddle.test"));
        assert!(command.is_none());

        let list = Cli::try_parse_from([
            "heddle",
            "auth",
            "invite",
            "list",
            "--server",
            "api.heddle.test",
        ])
        .expect("auth invite list flags parse");
        let Commands::Auth {
            command:
                AuthCommands::Invite {
                    email,
                    server,
                    command: Some(AuthInviteCommands::List),
                },
        } = list.command
        else {
            panic!("expected auth invite list");
        };
        assert_eq!(email, None);
        assert_eq!(server.as_deref(), Some("api.heddle.test"));

        assert!(
            Cli::try_parse_from([
                "heddle",
                "auth",
                "invite",
                "--email",
                "alice@example.com",
                "list",
            ])
            .is_err(),
            "create-only --email must not be accepted by list"
        );
    }

    #[test]
    fn derive_agent_parses_repeatable_scopes_and_operation_narrowing() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "derive-agent",
            "--server",
            "api.heddle.test",
            "--ttl",
            "900",
            "--scope",
            "repo:acme/api",
            "--scope",
            "namespace:acme",
            "--allow",
            "Push",
            "--allow",
            "GetState",
        ])
        .expect("derive-agent flags parse");

        let Commands::Auth {
            command:
                AuthCommands::DeriveAgent {
                    server,
                    ttl_secs,
                    scopes,
                    allowed_operations,
                    ..
                },
        } = cli.command
        else {
            panic!("expected auth derive-agent");
        };
        assert_eq!(server, "api.heddle.test");
        assert_eq!(ttl_secs, 900);
        assert_eq!(scopes, ["repo:acme/api", "namespace:acme"]);
        assert_eq!(allowed_operations, ["Push", "GetState"]);

        assert!(
            Cli::try_parse_from([
                "heddle",
                "auth",
                "derive-agent",
                "--server",
                "api.heddle.test",
                "--stdout",
            ])
            .is_err(),
            "token-only child export is unsafe because it cannot carry its proof key"
        );
    }

    #[test]
    fn derive_agent_parses_the_runner_persona_and_rejects_wider_overrides() {
        let cli = Cli::try_parse_from([
            "heddle",
            "auth",
            "derive-agent",
            "--server",
            "api.heddle.test",
            "--runner",
            "--scope",
            "spool:acme/api",
        ])
        .expect("runner flags parse");

        let Commands::Auth {
            command: AuthCommands::DeriveAgent { runner, scopes, .. },
        } = cli.command
        else {
            panic!("expected auth derive-agent");
        };
        assert!(runner);
        assert_eq!(scopes, ["spool:acme/api"]);

        for conflicting in [["--allow", "Push"], ["--template", "ci-landing"]] {
            assert!(
                Cli::try_parse_from([
                    "heddle",
                    "auth",
                    "derive-agent",
                    "--server",
                    "api.heddle.test",
                    "--runner",
                    conflicting[0],
                    conflicting[1],
                ])
                .is_err(),
                "runner persona must reject authority-changing overrides"
            );
        }
    }

    #[test]
    fn grant_create_parses_spool_principal_and_contributor_role() {
        let cli = Cli::try_parse_from([
            "heddle",
            "grant",
            "create",
            "--spool",
            "spool/willow-ibis-8e7264/notes",
            "--principal",
            "alice",
            "--role",
            "contributor",
            "--server",
            "api.preview.heddle.sh",
        ])
        .expect("grant create flags parse");
        let Commands::Grant {
            command: GrantCommands::Create(args),
        } = cli.command
        else {
            panic!("expected grant create");
        };
        assert_eq!(args.spool, "spool/willow-ibis-8e7264/notes");
        assert_eq!(args.principal, "alice");
        assert_eq!(args.role, GrantRoleArg::Contributor);
        assert_eq!(args.role.as_hosted_role_name(), "developer");
        assert!(args.role.as_grant_role().agent_may_grant());
        assert!(GrantRoleArg::Reader.as_grant_role().agent_may_grant());
        assert!(GrantRoleArg::Maintainer.as_grant_role().agent_may_grant());
        assert!(!GrantRoleArg::Admin.as_grant_role().agent_may_grant());
        assert!(!GrantRoleArg::Owner.as_grant_role().agent_may_grant());
        assert_eq!(args.server.as_deref(), Some("api.preview.heddle.sh"));
    }

    #[test]
    fn grant_list_and_delete_parse() {
        let list = Cli::try_parse_from([
            "heddle",
            "grant",
            "list",
            "--spool",
            "notes",
            "--server",
            "api.preview.heddle.sh",
        ])
        .expect("grant list flags parse");
        let Commands::Grant {
            command: GrantCommands::List(args),
        } = list.command
        else {
            panic!("expected grant list");
        };
        assert_eq!(args.spool, "notes");
        assert_eq!(args.server.as_deref(), Some("api.preview.heddle.sh"));

        let delete = Cli::try_parse_from([
            "heddle",
            "grant",
            "delete",
            "alice",
            "--spool",
            "spool/willow-ibis-8e7264/notes",
        ])
        .expect("grant delete flags parse");
        let Commands::Grant {
            command: GrantCommands::Delete(args),
        } = delete.command
        else {
            panic!("expected grant delete");
        };
        assert_eq!(args.id, "alice");
        assert_eq!(args.spool, "spool/willow-ibis-8e7264/notes");
        assert!(Cli::try_parse_from(["heddle", "grant", "delete", "alice"]).is_err());
    }
}
