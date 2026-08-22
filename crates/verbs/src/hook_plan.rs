// SPDX-License-Identifier: Apache-2.0
//! Pure hook install planning (no FS / stdin I/O).

/// How hook install obtains script bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInstallSourceKind {
    File,
    Stdin,
}

/// Pure install source plan after CLI gathers flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInstallSourcePlan {
    /// Proceed with the given source kind.
    Proceed(HookInstallSourceKind),
    /// Neither --from-file nor --from-stdin provided.
    SourceRequired,
    /// --from-stdin selected but content is empty.
    EmptyStdin,
}

/// Plan install source from pure flags + stdin emptiness.
///
/// `from_file` is true when a path was supplied (content validity is I/O).
/// `from_stdin` is true when stdin mode is selected.
/// `stdin_empty` is only meaningful when `from_stdin` is true.
pub fn plan_hook_install_source(
    from_file: bool,
    from_stdin: bool,
    stdin_empty: bool,
) -> HookInstallSourcePlan {
    if from_file {
        return HookInstallSourcePlan::Proceed(HookInstallSourceKind::File);
    }
    if from_stdin {
        if stdin_empty {
            return HookInstallSourcePlan::EmptyStdin;
        }
        return HookInstallSourcePlan::Proceed(HookInstallSourceKind::Stdin);
    }
    HookInstallSourcePlan::SourceRequired
}

/// Stable advice kind tokens for hook install refusals.
pub fn hook_install_source_required_kind() -> &'static str {
    "hook_install_source_required"
}

pub fn hook_install_empty_stdin_kind() -> &'static str {
    "hook_install_empty_stdin"
}

pub fn hook_unknown_kind() -> &'static str {
    "hook_unknown"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_source_plans() {
        assert_eq!(
            plan_hook_install_source(true, false, true),
            HookInstallSourcePlan::Proceed(HookInstallSourceKind::File)
        );
        assert_eq!(
            plan_hook_install_source(false, true, false),
            HookInstallSourcePlan::Proceed(HookInstallSourceKind::Stdin)
        );
        assert_eq!(
            plan_hook_install_source(false, true, true),
            HookInstallSourcePlan::EmptyStdin
        );
        assert_eq!(
            plan_hook_install_source(false, false, false),
            HookInstallSourcePlan::SourceRequired
        );
    }
}
