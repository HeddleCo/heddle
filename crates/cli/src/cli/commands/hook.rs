// SPDX-License-Identifier: Apache-2.0
//! Hook command - manage repository hooks.

use std::io::{self, IsTerminal, Read};

use anyhow::{Context, Result, anyhow};
use repo::{Hook, HookManager, Repository};

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, HookCommands, HookInstallSource, should_output_json};

pub fn cmd_hook(cli: &Cli, command: HookCommands) -> Result<()> {
    let repo = Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
    let manager = HookManager::new(&repo);

    match command {
        HookCommands::List => {
            let hooks = manager.list_hooks()?;

            if should_output_json(cli, Some(repo.config())) {
                println!("{}", serde_json::to_string(&hooks)?);
            } else if hooks.is_empty() {
                println!("No hooks installed");
            } else {
                for hook in hooks {
                    println!("{}", hook);
                }
            }
        }

        HookCommands::Install { name, source } => {
            let hook = Hook::from_name(&name).ok_or_else(|| hook_unknown_advice(&name))?;
            let content = load_hook_script(source)?;

            manager.install(hook, &content)?;

            if should_output_json(cli, Some(repo.config())) {
                println!("{{\"installed\": \"{}\"}}", name);
            } else {
                println!("Installed hook: {}", name);
            }
        }

        HookCommands::Uninstall { name } => {
            let hook = Hook::from_name(&name).ok_or_else(|| hook_unknown_advice(&name))?;

            let removed = manager.uninstall(hook)?;

            if should_output_json(cli, Some(repo.config())) {
                println!("{{\"uninstalled\": {}, \"name\": \"{}\"}}", removed, name);
            } else if removed {
                println!("Uninstalled hook: {}", name);
            } else {
                println!("Hook {} was not installed", name);
            }
        }

        HookCommands::Events { event } => {
            // W2/A15: print the static event catalog. Hardcoded names + a
            // brief description; the full JSON schemas live on the W2
            // gRPC service `HookService::GetHookEventSchema`.
            let catalog: &[(&str, &str)] = &[
                (
                    "pre_capture",
                    "fires before `heddle capture`; can add signals or abort",
                ),
                ("post_capture", "fires after a successful capture"),
                ("pre_merge", "fires before merge apply; can abort"),
                ("post_merge", "fires after a successful merge"),
                ("on_conflict", "fires on a conflict; can veto"),
                ("pre_thread_create", "fires before thread create; can abort"),
                ("post_thread_create", "fires after thread create"),
                ("pre_push", "fires before push; can abort"),
                ("post_push", "fires after push"),
                ("on_signal", "fires when a risk signal is recorded"),
            ];
            let filtered: Vec<&(&str, &str)> = if let Some(name) = event.as_deref() {
                catalog.iter().filter(|(n, _)| *n == name).collect()
            } else {
                catalog.iter().collect()
            };
            if should_output_json(cli, Some(repo.config())) {
                let entries: Vec<_> = filtered
                    .iter()
                    .map(|(name, desc)| serde_json::json!({"name": name, "description": desc}))
                    .collect();
                println!("{}", serde_json::json!({"events": entries}));
            } else if filtered.is_empty() {
                println!("(no matching events)");
            } else {
                for (name, desc) in &filtered {
                    println!("  {name:24} {desc}");
                }
            }
        }
    }

    Ok(())
}

fn load_hook_script(source: HookInstallSource) -> Result<String> {
    if let Some(path) = source.from_file {
        return std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read hook script from {}", path.display()));
    }

    if source.from_stdin {
        return read_hook_stdin().context("failed to read hook script from stdin");
    }

    if !io::stdin().is_terminal() {
        return read_hook_stdin().context("failed to read hook script from stdin");
    }

    Err(anyhow!(hook_install_source_required_advice()))
}

fn read_hook_stdin() -> Result<String> {
    let mut content = String::new();
    io::stdin().read_to_string(&mut content)?;
    if content.is_empty() {
        return Err(anyhow!(hook_install_empty_stdin_advice()));
    }
    Ok(content)
}

fn hook_unknown_advice(name: &str) -> anyhow::Error {
    anyhow!(RecoveryAdvice::safety_refusal(
        "hook_unknown",
        format!("Unknown hook: {name}"),
        "Inspect supported hooks with `heddle hook events`, then retry with one of those names.",
        format!("hook '{name}' is not registered in the hook event catalog"),
        "installing or uninstalling an unknown hook would create policy state the runtime never executes",
        "no hook files or hook metadata were changed",
        "heddle hook events",
        vec!["heddle hook events".to_string()],
    ))
}

fn hook_install_source_required_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "hook_install_source_required",
        "hook install requires --from-file <path> or stdin input",
        "Pass `--from-file <path>` or pipe hook content with `--from-stdin`.",
        "hook install was invoked without a script source",
        "installing an empty or implicit hook would create policy behavior the user did not provide",
        "no hook file was written",
        "heddle hook install <name> --from-file <path>",
        vec!["heddle hook install <name> --from-file <path>".to_string()],
    )
}

fn hook_install_empty_stdin_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "hook_install_empty_stdin",
        "hook install received empty stdin; pass --from-file <path> or pipe script content",
        "Pass `--from-file <path>` or pipe non-empty hook content with `--from-stdin`.",
        "hook install read an empty stdin stream",
        "installing an empty hook could silently replace expected repository policy",
        "no hook file was written",
        "heddle hook install <name> --from-file <path>",
        vec!["heddle hook install <name> --from-file <path>".to_string()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_hook_script_errors_when_no_source_is_provided() {
        let err = load_hook_script(HookInstallSource {
            from_file: None,
            from_stdin: false,
        })
        .expect_err("missing source should fail");
        let message = err.to_string();
        assert!(
            message.contains("hook install requires --from-file <path> or stdin input")
                || message.contains("failed to read hook script from stdin")
                || message.contains("received empty stdin"),
            "unexpected error: {message}"
        );
    }
}
