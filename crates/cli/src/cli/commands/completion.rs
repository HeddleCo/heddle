// SPDX-License-Identifier: Apache-2.0
//! Completion command - generate shell completion scripts.

use anyhow::{Result, anyhow};
use clap::CommandFactory;
use clap_complete::{Shell, generate};
use heddle_core::completion_plan::{CompletionShell, CompletionShellError, parse_completion_shell};

use super::advice::RecoveryAdvice;
use crate::cli::{Cli, CompletionSubject};

pub fn cmd_completion(shell: String) -> Result<()> {
    let mut cmd = Cli::command();

    match parse_completion_shell(&shell) {
        Ok(CompletionShell::Bash) => {
            generate(Shell::Bash, &mut cmd, "heddle", &mut std::io::stdout());
            print!("{BASH_DYNAMIC_COMPLETION}");
        }
        Ok(CompletionShell::Zsh) => {
            generate(Shell::Zsh, &mut cmd, "heddle", &mut std::io::stdout());
            print!("{ZSH_DYNAMIC_COMPLETION}");
        }
        Ok(CompletionShell::Fish) => {
            generate(Shell::Fish, &mut cmd, "heddle", &mut std::io::stdout());
            print!("{FISH_DYNAMIC_COMPLETION}");
        }
        Err(CompletionShellError::Unsupported { shell }) => {
            // Recovery copy lives in the CLI, not core.
            return Err(anyhow!(RecoveryAdvice::invalid_usage(
                "completion_shell_unsupported",
                format!("Unsupported shell: {shell}. Supported shells: bash, zsh, fish"),
                "Use one of: bash, zsh, fish.",
                "heddle shell completion bash",
            )));
        }
    }

    Ok(())
}

pub fn cmd_complete(cli: &Cli, subject: CompletionSubject) -> Result<()> {
    let Ok(repo) = cli.open_repo() else {
        return Ok(());
    };
    let mut names: Vec<String> = match subject {
        CompletionSubject::Threads => repo
            .refs()
            .list_threads()?
            .into_iter()
            .map(|name| name.to_string())
            .collect(),
        CompletionSubject::Markers => repo
            .refs()
            .list_markers()?
            .into_iter()
            .map(|name| name.to_string())
            .collect(),
    };
    names.sort();
    names.dedup();
    for name in names {
        println!("{name}");
    }
    Ok(())
}

const BASH_DYNAMIC_COMPLETION: &str = r#"

# Dynamic Heddle completions layered on top of the clap-generated surface.
__heddle_dynamic_words() {
    command heddle __complete "$1" 2>/dev/null
}

__heddle_complete_from() {
    local subject="$1"
    mapfile -t COMPREPLY < <(compgen -W "$(__heddle_dynamic_words "$subject")" -- "${COMP_WORDS[COMP_CWORD]}")
}

__heddle_thread_value_position() {
    local prev="${COMP_WORDS[COMP_CWORD-1]}"
    case "$prev" in
        --thread) return 0 ;;
        --into)
            case "${COMP_WORDS[1]}" in
                thread|capture) return 0 ;;
            esac
            ;;
    esac
    case "${COMP_WORDS[1]}" in
        start) [ "$COMP_CWORD" -eq 2 ] && return 0 ;;
    esac
    if [ "${COMP_WORDS[1]}" = "thread" ]; then
        case "${COMP_WORDS[2]}" in
            switch|cd|show|captures|refresh|promote|drop|delete|absorb|resolve|approve|approvals|check-merge)
                [ "$COMP_CWORD" -eq 3 ] && return 0
                ;;
            rename|move)
                [ "$COMP_CWORD" -eq 3 ] || [ "$COMP_CWORD" -eq 4 ] && return 0
                ;;
        esac
    fi
    return 1
}

__heddle_marker_value_position() {
    if [ "${COMP_WORDS[1]}" = "thread" ] && [ "${COMP_WORDS[2]}" = "marker" ]; then
        case "${COMP_WORDS[3]}" in
            show|delete) [ "$COMP_CWORD" -eq 4 ] && return 0 ;;
        esac
    fi
    return 1
}

__heddle_dynamic_complete() {
    if __heddle_thread_value_position; then
        __heddle_complete_from threads
        return 0
    fi
    if __heddle_marker_value_position; then
        __heddle_complete_from markers
        return 0
    fi
    _heddle "$@"
}

complete -F __heddle_dynamic_complete -o bashdefault -o default heddle
"#;

const ZSH_DYNAMIC_COMPLETION: &str = r#"

# Dynamic Heddle completions layered on top of the clap-generated surface.
__heddle_dynamic_values() {
    local subject="$1"
    local -a values
    values=("${(@f)$(command heddle __complete "$subject" 2>/dev/null)}")
    _describe "$subject" values
}

__heddle_thread_value_position() {
    local prev="${words[$CURRENT-1]}"
    case "$prev" in
        --thread) return 0 ;;
        --into)
            case "${words[2]}" in
                thread|capture) return 0 ;;
            esac
            ;;
    esac
    case "${words[2]}" in
        start) [[ "$CURRENT" -eq 3 ]] && return 0 ;;
    esac
    if [[ "${words[2]}" == "thread" ]]; then
        case "${words[3]}" in
            switch|cd|show|captures|refresh|promote|drop|delete|absorb|resolve|approve|approvals|check-merge)
                [[ "$CURRENT" -eq 4 ]] && return 0
                ;;
            rename|move)
                [[ "$CURRENT" -eq 4 || "$CURRENT" -eq 5 ]] && return 0
                ;;
        esac
    fi
    return 1
}

__heddle_marker_value_position() {
    if [[ "${words[2]}" == "thread" && "${words[3]}" == "marker" ]]; then
        case "${words[4]}" in
            show|delete) [[ "$CURRENT" -eq 5 ]] && return 0 ;;
        esac
    fi
    return 1
}

__heddle_dynamic_complete() {
    if __heddle_thread_value_position; then
        __heddle_dynamic_values threads
        return
    fi
    if __heddle_marker_value_position; then
        __heddle_dynamic_values markers
        return
    fi
    _heddle "$@"
}

compdef __heddle_dynamic_complete heddle
"#;

const FISH_DYNAMIC_COMPLETION: &str = r#"

# Dynamic Heddle completions layered on top of the clap-generated surface.
function __heddle_dynamic_subject
    set -l words (commandline -opc)
    set -l prev ''
    if test (count $words) -gt 0
        set prev $words[-1]
    end
    switch $prev
        case --thread
            printf threads
            return 0
        case --into
            if __fish_seen_subcommand_from thread capture
                printf threads
                return 0
            end
    end
    if test (count $words) -eq 2
        switch "$words[2]"
            case start
                printf threads
                return 0
        end
    end
    if test (count $words) -ge 3; and test "$words[2]" = thread
        switch "$words[3]"
            case switch cd show captures refresh promote drop delete absorb resolve approve approvals check-merge
                if test (count $words) -eq 3
                    printf threads
                    return 0
                end
            case rename move
                if test (count $words) -eq 3; or test (count $words) -eq 4
                    printf threads
                    return 0
                end
        end
    end
    if test (count $words) -eq 4; and test "$words[2]" = thread; and test "$words[3]" = marker
        switch "$words[4]"
            case show delete
                printf markers
                return 0
        end
    end
end

complete -c heddle -n 'set -l subject (__heddle_dynamic_subject); test "$subject" = threads' -f -a '(command heddle __complete threads 2>/dev/null)'
complete -c heddle -n 'set -l subject (__heddle_dynamic_subject); test "$subject" = markers' -f -a '(command heddle __complete markers 2>/dev/null)'
"#;
