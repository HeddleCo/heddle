// SPDX-License-Identifier: Apache-2.0
//! Heddle: An AI-native version control system.

use std::{any::Any, time::Instant};

use anyhow::Result;
use clap::{Arg, ArgAction, CommandFactory, Parser, error::ErrorKind};
#[cfg(all(feature = "git-overlay", feature = "ingest"))]
use cli::cli::commands::cmd_context_reason_git;
#[cfg(feature = "semantic")]
use cli::cli::commands::cmd_semantic;
#[cfg(feature = "git-overlay")]
use cli::cli::{
    ExportCommands, ImportCommands,
    cli_args::SyncCommands,
    commands::{cmd_export_git, cmd_import_git, cmd_sync_git},
};
use cli::{
    cli::{
        AgentCommands, Cli, CloneArgs, CollapseArgs, Commands, ContextCommands, DaemonCommands,
        DiffArgs, ExpandArgs, IntegrationCommands, LogArgs, ResolveArgs, RetroArgs, RevertArgs,
        RunArgs, UndoArgs,
        cli_args::{LandArgs, SyncArgs},
        commands::{
            LogCommandOptions, RetroCommandOptions, SnapshotAgentOverrides, build_command_catalog,
            cmd_abort, cmd_adopt, cmd_agent, cmd_capture_split, cmd_clone, cmd_collapse,
            cmd_commit, cmd_complete, cmd_context_audit, cmd_context_check, cmd_context_edit,
            cmd_context_get, cmd_context_history, cmd_context_list, cmd_context_rm,
            cmd_context_set, cmd_context_suggest, cmd_context_supersede, cmd_continue,
            cmd_daemon_serve, cmd_daemon_status, cmd_daemon_stop, cmd_diff, cmd_discuss,
            cmd_doctor, cmd_doctor_docs, cmd_doctor_schemas, cmd_expand, cmd_fsck,
            cmd_fsck_repair_git, cmd_hook, cmd_init, cmd_integration, cmd_land, cmd_log,
            cmd_maintenance, cmd_oplog, cmd_pull, cmd_push, cmd_query, cmd_ready, cmd_redo,
            cmd_remote, cmd_resolve, cmd_retro, cmd_revert, cmd_review, cmd_run, cmd_schemas,
            cmd_shell, cmd_show, cmd_snapshot, cmd_start, cmd_status, cmd_sync_smart, cmd_thread,
            cmd_timeline, cmd_try, cmd_undo, cmd_verify, cmd_watch,
            command_runtime_contract_for_command, print_error_with_hint,
            print_parse_error_json_envelope,
        },
        render::write_json_stdout,
    },
    config::UserConfig,
    exit::HeddleExitCode,
    logging::{LoggingConfig, init_logging},
    operation_id::{resolve_operation_id, run_local_idempotency_if_requested},
    perf::{ProfileField, emit_command_profile, profile_enabled},
};
use tracing::debug;

// `current_thread` flavor avoids spinning up a CPU-count-sized worker
// pool on every CLI invocation. The foreground `heddle` binary is a
// one-shot command — `heddle status`, `heddle capture`, etc. don't
// fan out across cores. Daemon variants (`heddle daemon serve`,
// `heddle agent serve`) override this with their own runtime setup
// when they need real concurrency. Saves ~10-30ms of startup that the
// multi-thread flavor pays for thread-pool creation + teardown.
fn main() -> Result<()> {
    install_broken_pipe_panic_hook();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(async_main())
    }));
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) if is_broken_pipe_error(&error) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(payload) if is_broken_pipe_panic(payload.as_ref()) => Ok(()),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

async fn async_main() -> Result<()> {
    // Install the ring crypto provider as the rustls default. Without this,
    // any rustls TLS handshake (gRPC, GitHub REST, `import git
    // https://…`) panics in 0.23.x. We pin ring instead of aws-lc-rs to
    // keep the 80s aws-lc-sys C build out of release builds. Measured
    // ~0ms on macOS — defensive ordering rather than a perf hot spot.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Register lazy-clone hydrator factories with the `repo` crate's
    // global registry. This must happen before any `Repository::open`
    // call so that opening a lazy-cloned repo can reconstruct + install
    // the on-read blob hydrator transparently. Without these
    // registrations, the second-and-subsequent CLI invocation against a
    // `--lazy` clone would see `MissingObject` on every blob read.
    cli::cli::commands::register_git_overlay_factory();
    #[cfg(feature = "client")]
    heddle_client::grpc_hosted::register_hosted_factory();

    // Pick the hosted authentication implementation at startup.
    #[cfg(feature = "client")]
    let hosted: Box<dyn weft_client_shim::WeftExtensions> =
        Box::new(cli::extensions::EnabledWeftExtensions);
    // OSS builds dispatch no hosted commands (those `Commands` variants
    // are gated behind `client`), so the trait object is unused
    // and we drop the binding entirely. Keeping the shim trait + Noop
    // visible for downstream consumers and post-split closed builds.

    let total_start = Instant::now();
    let profile = profile_enabled();
    // Intercept the bare-help shapes BEFORE clap parses, so we
    // serve the curated everyday list instead of clap's auto-help.
    // Catches `heddle`, `heddle --help`, `heddle -h`, `heddle help`,
    // AND the case where only global flags were passed (e.g.
    // `heddle --output text`). Without the global-flags branch, clap
    // emits its 60+ verb wall-of-text on missing subcommand — which is
    // exactly the noisy first impression the curated printer is meant
    // to replace.
    {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let bare = raw.is_empty()
            || raw == ["--help"]
            || raw == ["-h"]
            || raw == ["help"]
            || is_global_flags_only(&raw);
        if bare {
            let command_start = Instant::now();
            if raw_wants_json(&raw) {
                write_json_stdout(&build_command_catalog())?;
            } else {
                cli::cli::help::print_help(&Cli::command(), &[])?;
            }
            if profile {
                emit_command_profile(
                    "help",
                    0,
                    &[
                        ProfileField::duration("command_body_ms", command_start.elapsed()),
                        ProfileField::duration("total_ms", total_start.elapsed()),
                    ],
                );
            }
            return Ok(());
        }
        if let Some(result) = cli::cli::help::print_direct_help_for_raw(&Cli::command(), &raw) {
            result?;
            if profile {
                emit_command_profile(
                    "help",
                    0,
                    &[ProfileField::duration("total_ms", total_start.elapsed())],
                );
            }
            return Ok(());
        }
        // `heddle help <topic>` — let clap handle when the user passes
        // the verb explicitly (it dispatches to Commands::Help). A two-
        // arg form `heddle help <topic>` also goes through clap.
    }
    let raw_argv: Vec<String> = std::env::args().collect();
    let cli = match Cli::try_parse_from(raw_argv) {
        Ok(cli) => cli,
        Err(err) => {
            let raw: Vec<String> = std::env::args().skip(1).collect();
            if raw_wants_json(&raw)
                && !matches!(
                    err.kind(),
                    ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
                )
            {
                print_parse_error_json_envelope(&err);
                std::process::exit(HeddleExitCode::from_clap(&err).into());
            }
            err.print()?;
            std::process::exit(HeddleExitCode::from_clap(&err).into());
        }
    };
    // `heddle capture --help-agent`: clap has now parsed the entire command
    // line — every global spelling it accepts (`-C <path>`, `--output <fmt>`,
    // clustered `-vC <path>`, attached forms, any position) was handled by
    // clap, not a hand-rolled token scan. Inspect the parsed result and render
    // the reveal help before running capture. This is help, not diagnostics,
    // so it exits before config/logging init.
    if let Commands::Capture(args) = &cli.command
        && args.help_agent
    {
        cli::cli::help::print_capture_agent_help(&Cli::command())?;
        if profile {
            emit_command_profile(
                "help",
                0,
                &[ProfileField::duration("total_ms", total_start.elapsed())],
            );
        }
        return Ok(());
    }
    // Resolve color decision once, before any rendering site fires.
    // The helpers in `cli::style` consult a process-wide OnceLock —
    // doing this inside each render path would re-query the env on
    // every line and fight the brand goal of restraint.
    cli::cli::style::init_from_cli(&cli);
    let command_contract = command_runtime_contract_for_command(&cli.command);
    let command_name = command_contract.display.clone();
    let command_supports_op_id = command_contract.supports_op_id;
    let config_start = Instant::now();
    // Route early UserConfig load failures through the same typed
    // envelope as command-body errors. Without this, a legacy
    // `output.format = "auto"` in the global user config (or via
    // `HEDDLE_CONFIG`) exits with a raw TOML parse error and bypasses
    // the `Next:` / JSON-envelope contract #271 promised — `?` here
    // would propagate to `main` and print via anyhow's Debug impl.
    let user_config = match UserConfig::load_default() {
        Ok(config) => config,
        // Hidden harness relay hooks must reach harness init so that a bad
        // user config is reported as a warning and the hook can continue.
        // Normal foreground commands keep the strict typed error path.
        Err(_) if is_harness_relay_invocation(&cli.command) => UserConfig::default(),
        Err(err) => {
            let code = HeddleExitCode::from_error(&err);
            print_error_with_hint(&cli, &err);
            std::process::exit(code.into());
        }
    };
    let config_load_ms = config_start.elapsed().as_millis();
    let logging_start = Instant::now();
    // Foreground CLI commands default to WARN-level logs so the human-facing
    // surface stays quiet. Long-running daemons keep the historical INFO
    // default since their stderr is the operator's audit log.
    let base_logging = LoggingConfig::from_user_and_env(Some(&user_config));
    let logging = if is_daemon_invocation(&cli.command) {
        base_logging.with_verbosity(cli.verbose.max(1), cli.quiet)
    } else {
        base_logging.with_verbosity(cli.verbose, cli.quiet)
    };
    let telemetry = init_logging(logging);
    let logging_init_ms = logging_start.elapsed().as_millis();

    debug!(
        command = command_name.as_str(),
        config_load_ms,
        logging_init_ms,
        startup_ms = total_start.elapsed().as_millis(),
        "CLI startup complete"
    );

    // Unsupported-output gates exit through `from_error` so the process
    // exit code and the envelope's `exit_code` field agree: DataErr (65),
    // because `--output json[-compact]` here is well-formed syntax the
    // command semantically rejects — not a malformed invocation (Usage 64).
    // Agents treat 64 as "fix your argv" and retry-with-mutation; 65 tells
    // them to fall back to a supported output mode (HeddleCo/heddle#648).
    if explicit_json_requested(&cli) && !command_contract.supports_json {
        telemetry.shutdown();
        let err = anyhow::anyhow!(cli::cli::commands::RecoveryAdvice::json_unsupported(
            &command_name
        ));
        let code = HeddleExitCode::from_error(&err);
        print_error_with_hint(&cli, &err);
        std::process::exit(code.into());
    }
    if cli::cli::output_is_compact(&cli) && !command_contract.supports_json_compact {
        telemetry.shutdown();
        let err = anyhow::anyhow!(
            cli::cli::commands::RecoveryAdvice::json_compact_unsupported(&command_name)
        );
        let code = HeddleExitCode::from_error(&err);
        print_error_with_hint(&cli, &err);
        std::process::exit(code.into());
    }

    match run_local_idempotency_if_requested(&cli, &command_name, command_supports_op_id) {
        Ok(true) => {
            telemetry.shutdown();
            return Ok(());
        }
        Ok(false) => {}
        Err(err) => {
            telemetry.shutdown();
            let code = HeddleExitCode::from_error(&err);
            print_error_with_hint(&cli, &err);
            std::process::exit(code.into());
        }
    }

    if command_supports_op_id {
        resolve_operation_id(&cli)?;
    }

    let command_start = Instant::now();
    let result = match &cli.command {
        Commands::Init(args) => cmd_init(&cli, args.clone()),

        Commands::Adopt(args) => cmd_adopt(&cli, args.clone()),

        Commands::Help { topics } => {
            // Curated help printer. No op-id (read-only), no
            // structured output unless explicitly asked to print the
            // command catalog.
            if explicit_json_requested(&cli) {
                write_json_stdout(&build_command_catalog())
            } else {
                cli::cli::help::print_help(&Cli::command(), topics).map_err(Into::into)
            }
        }

        Commands::Status {
            short,
            watch,
            watch_iterations,
            watch_interval_ms,
        } => cmd_status(&cli, *short, *watch, *watch_iterations, *watch_interval_ms).await,

        Commands::Watch(args) => cmd_watch(&cli, args.clone()).await,

        Commands::Verify => cmd_verify(&cli, cli.verbose > 0),

        Commands::Doctor(args) => match &args.command {
            None => cmd_doctor(&cli, args.profile),
            Some(cli::cli::DoctorCommands::Docs(docs_args)) => {
                cmd_doctor_docs(&cli, docs_args.clone())
            }
            Some(cli::cli::DoctorCommands::Schemas(schema_args)) => {
                cmd_doctor_schemas(&cli, schema_args.clone())
            }
        },

        Commands::Schemas { verb } => cmd_schemas(&cli, verb),

        Commands::Start(args) => cmd_start(&cli, args.clone()),

        Commands::Run(RunArgs { thread, command }) => {
            cmd_run(&cli, thread.clone(), command.clone())
        }

        Commands::Try(args) => cmd_try(&cli, args.clone()),

        Commands::Sync(args) => {
            #[cfg(feature = "git-overlay")]
            {
                if let Some(SyncCommands::Git { path }) = &args.command {
                    cmd_sync_git(&cli, path.clone())
                } else {
                    // Codex's enhanced sync (rebase-aware fast-forward path);
                    // main wired `cmd_sync` here pre-rebase. The smart variant
                    // is a strict superset, so we use it on the merged
                    // branch.
                    cmd_sync_smart(
                        &cli,
                        SyncArgs {
                            command: None,
                            thread: args.thread.clone(),
                        },
                    )
                    .await
                }
            }
            #[cfg(not(feature = "git-overlay"))]
            {
                cmd_sync_smart(&cli, args.clone()).await
            }
        }

        Commands::Continue => cmd_continue(&cli).await,

        Commands::Abort => cmd_abort(&cli),

        Commands::Land(LandArgs {
            thread,
            message,
            no_squash,
        }) => {
            cmd_land(
                &cli,
                LandArgs {
                    thread: thread.clone(),
                    message: message.clone(),
                    no_squash: *no_squash,
                },
            )
            .await
        }

        Commands::Ready(args) => cmd_ready(&cli, args.clone()).await,

        Commands::Capture(args) => {
            if args.split {
                cmd_capture_split(
                    &cli,
                    args.into.clone().unwrap_or_default(),
                    args.paths.clone(),
                    args.intent.clone(),
                )
            } else {
                cmd_snapshot(
                    &cli,
                    args.intent.clone(),
                    args.confidence,
                    args.force,
                    SnapshotAgentOverrides {
                        provider: args.agent_provider.clone(),
                        model: args.agent_model.clone(),
                        session: args.agent_session.clone(),
                        segment: args.agent_segment.clone(),
                        policy: args.policy.clone(),
                        no_policy: args.no_policy,
                        no_agent: args.no_agent,
                    },
                )
                .await
            }
        }

        Commands::Commit(args) => cmd_commit(&cli, args.clone()),

        Commands::Log(LogArgs {
            state,
            limit,
            all,
            graph,
            oneline,
            reflog,
            timeline,
            thread,
            agent,
            paths,
            since,
        }) => {
            cmd_log(
                &cli,
                LogCommandOptions {
                    state: state.clone(),
                    limit: *limit,
                    all: *all,
                    graph: *graph,
                    oneline: *oneline,
                    reflog: *reflog,
                    timeline: *timeline,
                    thread: thread.clone(),
                    agent: agent.clone(),
                    paths: paths.clone(),
                    since: since.clone(),
                },
            )
            .await
        }

        Commands::Show { state } => cmd_show(&cli, state.clone()),

        Commands::Timeline(args) => cmd_timeline(&cli, args.clone()),

        Commands::Retro(RetroArgs {
            since,
            include_merges,
            include_undos,
            full,
        }) => {
            cmd_retro(
                &cli,
                RetroCommandOptions {
                    since: since.clone(),
                    include_merges: *include_merges,
                    include_undos: *include_undos,
                    verbose: *full,
                },
            )
            .await
        }

        Commands::Diff(DiffArgs {
            from,
            to,
            semantic,
            stat,
            name_only,
            unified,
            context,
            patch,
        }) => cmd_diff(
            &cli,
            from.clone(),
            to.clone(),
            *semantic,
            *stat,
            *name_only,
            *unified,
            *context,
            *patch,
        ),

        Commands::Revert(RevertArgs {
            state,
            message,
            no_commit,
        }) => cmd_revert(&cli, state.clone(), message.clone(), *no_commit),

        Commands::Undo(UndoArgs {
            steps,
            list,
            depth,
            preview,
            redo,
            allow_redact_undo,
        }) => {
            if *redo {
                cmd_redo(&cli, *steps, *preview)
            } else {
                cmd_undo(&cli, *steps, *list, *depth, *preview, *allow_redact_undo)
            }
        }

        #[cfg(feature = "git-overlay")]
        Commands::Import { command } => match command {
            ImportCommands::Git { path, refs, lossy } => {
                cmd_import_git(&cli, path.clone(), refs.clone(), *lossy)
            }
        },

        #[cfg(feature = "git-overlay")]
        Commands::Export { command } => match command {
            ExportCommands::Git { destination } => cmd_export_git(&cli, destination.clone()),
        },

        Commands::Fsck(args) => match &args.command {
            None => cmd_fsck(&cli, args.full, args.thorough, args.git),
            Some(cli::cli::FsckCommands::Repair { target }) => match target {
                cli::cli::FsckRepairCommands::Git(args) => cmd_fsck_repair_git(
                    &cli,
                    args.ref_name.clone(),
                    args.prefer.clone(),
                    args.preview,
                ),
            },
        },

        Commands::Oplog { command } => cmd_oplog(&cli, command.clone()),

        Commands::Collapse(CollapseArgs {
            states,
            into,
            confidence,
        }) => cmd_collapse(&cli, states.clone(), into.clone(), *confidence),

        Commands::Expand(ExpandArgs { reference }) => cmd_expand(&cli, reference.clone()),

        Commands::Thread { command } => cmd_thread(&cli, command.clone()).await,

        Commands::Shell { command } => cmd_shell(&cli, command.clone()),

        Commands::Complete { subject } => cmd_complete(&cli, *subject),

        Commands::Resolve(ResolveArgs {
            path,
            all,
            list,
            ours,
            theirs,
            force,
            abort,
        }) => cmd_resolve(
            &cli,
            path.clone(),
            *all,
            *list,
            *ours,
            *theirs,
            *force,
            *abort,
        ),

        Commands::Push(args) => {
            cmd_push(
                &cli,
                args.remote.clone(),
                args.thread_name(),
                args.state.clone(),
                args.force,
                args.all_threads,
                args.insecure,
            )
            .await
        }

        Commands::Pull(args) => {
            cmd_pull(
                &cli,
                args.remote_op.remote.clone(),
                args.remote_op.thread.clone(),
                args.local_thread.clone(),
                args.lazy,
                args.remote_op.insecure,
            )
            .await
        }

        Commands::Remote { command } => cmd_remote(&cli, command.clone()),

        #[cfg(feature = "client")]
        Commands::Auth { command } => {
            let cmd = command.clone();
            hosted.auth(&cli, &cmd).await
        }

        Commands::Context { command } => match command {
            ContextCommands::Set(args) => {
                cmd_context_set(
                    &cli,
                    args.target.path.clone(),
                    args.target.state.clone(),
                    args.scope.clone(),
                    args.kind.clone(),
                    args.tag.clone(),
                    args.message.clone(),
                    args.file.clone(),
                )
                .await
            }
            ContextCommands::Get(args) => {
                cmd_context_get(
                    &cli,
                    args.target.path.clone(),
                    args.target.state.clone(),
                    args.scope.clone(),
                    args.tag.clone(),
                    args.r#ref.clone(),
                )
                .await
            }
            ContextCommands::List(args) => {
                cmd_context_list(
                    &cli,
                    args.prefix.clone(),
                    args.tag.clone(),
                    args.r#ref.clone(),
                    args.include_superseded,
                )
                .await
            }
            ContextCommands::History(args) => {
                cmd_context_history(&cli, args.annotation_id.clone(), args.r#ref.clone()).await
            }
            ContextCommands::Edit(args) => {
                cmd_context_edit(
                    &cli,
                    args.annotation_id.clone(),
                    args.kind.clone(),
                    args.tag.clone(),
                    args.message.clone(),
                    args.file.clone(),
                )
                .await
            }
            ContextCommands::Supersede(args) => {
                cmd_context_supersede(
                    &cli,
                    args.annotation_id.clone(),
                    args.target.path.clone(),
                    args.target.state.clone(),
                    args.scope.clone(),
                    args.kind.clone(),
                    args.tag.clone(),
                    args.message.clone(),
                    args.file.clone(),
                )
                .await
            }
            ContextCommands::Rm(args) => {
                cmd_context_rm(
                    &cli,
                    args.target.path.clone(),
                    args.target.state.clone(),
                    args.scope.clone(),
                    args.all,
                )
                .await
            }
            ContextCommands::Check(args) => {
                cmd_context_check(
                    &cli,
                    args.path.clone(),
                    args.state.clone(),
                    args.tag.clone(),
                    args.r#ref.clone(),
                )
                .await
            }
            ContextCommands::Suggest(args) => {
                cmd_context_suggest(&cli, args.r#ref.clone(), args.limit).await
            }
            ContextCommands::Audit(args) => cmd_context_audit(&cli, args.r#ref.clone()).await,
            #[cfg(all(feature = "git-overlay", feature = "ingest"))]
            ContextCommands::Reason { command } => match command {
                cli::cli::cli_args::ContextReasonCommands::Git(args) => cmd_context_reason_git(
                    &cli,
                    &args.path,
                    args.max_sessions_per_commit,
                    args.min_match_confidence,
                    args.limit,
                    args.claude_home.clone(),
                    args.codex_home.clone(),
                    args.opencode_home.clone(),
                    args.dry_run,
                ),
            },
        },

        Commands::Integration { command } => cmd_integration(&cli, command.clone()),

        #[cfg(feature = "semantic")]
        Commands::Semantic { command } => cmd_semantic(&cli, command.clone()),

        Commands::Daemon { command } => match command {
            DaemonCommands::Serve => cmd_daemon_serve(&cli),
            DaemonCommands::Status => cmd_daemon_status(&cli),
            DaemonCommands::Stop => cmd_daemon_stop(&cli),
        },

        Commands::Agent { command } => cmd_agent(&cli, command).await,

        Commands::Discuss { command } => cmd_discuss(&cli, command).await,

        Commands::Query(args) => cmd_query(&cli, args).await,

        Commands::Review { command } => cmd_review(&cli, command).await,

        Commands::Redact { command } => cli::cli::commands::cmd_redact(&cli, command.clone()),

        Commands::Visibility { command } => {
            cli::cli::commands::cmd_visibility(&cli, command.clone())
        }

        Commands::Maintenance { command } => cmd_maintenance(&cli, command.clone()),

        Commands::Clone(CloneArgs {
            remote,
            local,
            thread,
            depth,
            lazy,
            filter,
            recursive,
            insecure,
        }) => {
            cmd_clone(
                &cli,
                remote.clone(),
                local.clone(),
                thread.clone(),
                *depth,
                *lazy,
                filter.clone(),
                *recursive,
                *insecure,
            )
            .await
        }

        Commands::Hook { command } => cmd_hook(&cli, command.clone()),
    };

    debug!(
        command = command_name.as_str(),
        config_load_ms,
        logging_init_ms,
        command_body_ms = command_start.elapsed().as_millis(),
        total_ms = total_start.elapsed().as_millis(),
        "CLI command complete"
    );

    if profile {
        let exit_status = match &result {
            Ok(()) => 0,
            Err(err) if is_broken_pipe_error(err) => 0,
            Err(err) => HeddleExitCode::from_error(err).into(),
        };
        emit_command_profile(
            &command_name,
            exit_status,
            &[
                ProfileField::millis("config_load_ms", config_load_ms),
                ProfileField::millis("logging_init_ms", logging_init_ms),
                ProfileField::duration("command_body_ms", command_start.elapsed()),
                ProfileField::duration("total_ms", total_start.elapsed()),
            ],
        );
    }

    telemetry.shutdown();
    match result {
        Ok(()) => Ok(()),
        Err(err) if is_broken_pipe_error(&err) => Ok(()),
        Err(err) => {
            let code = HeddleExitCode::from_error(&err);
            // OutcomeExit means the command already rendered its report
            // (operator envelope, eligibility output, …); skip a second
            // error envelope so JSON/text stay single-stream contracts.
            if !HeddleExitCode::is_quiet_outcome(&err) {
                print_error_with_hint(&cli, &err);
            }
            std::process::exit(code.into());
        }
    }
}

fn is_harness_relay_invocation(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Integration {
            command: IntegrationCommands::Relay(_),
        }
    )
}

/// True when the raw argv (after the program name) contains only global
/// flags and their values — i.e. the user typed `heddle --output text` or
/// `heddle --no-color -v` with no subcommand verb. We want to show the
/// curated everyday-verb help in that case, not clap's wall-of-subcommands
/// error.
///
/// The global flag set comes from clap metadata so this pre-parse fast path
/// tracks the real CLI contract, including hidden globals and aliases.
fn is_global_flags_only(raw: &[String]) -> bool {
    if raw.is_empty() {
        return false; // caller already handles the truly-empty case
    }

    let command = Cli::command();
    raw_global_flags(&command, raw).is_some()
}

fn raw_wants_json(raw: &[String]) -> bool {
    let command = Cli::command();
    let mut wants_json = false;
    let mut index = 0;

    while index < raw.len() {
        let Some((arg, value, consumed)) = raw_global_flag_at(&command, raw, index) else {
            index += 1;
            continue;
        };
        if arg.get_id().as_str() == "output" && value.is_some_and(|value| value == "json") {
            wants_json = true;
        }
        index += consumed;
    }

    wants_json
}

fn raw_global_flags<'a>(
    command: &'a clap::Command,
    raw: &'a [String],
) -> Option<Vec<(&'a Arg, Option<&'a str>)>> {
    let mut flags = Vec::new();
    let mut index = 0;
    while index < raw.len() {
        let (arg, value, consumed) = raw_global_flag_at(command, raw, index)?;
        flags.push((arg, value));
        index += consumed;
    }
    Some(flags)
}

fn raw_global_flag_at<'a>(
    command: &'a clap::Command,
    raw: &'a [String],
    index: usize,
) -> Option<(&'a Arg, Option<&'a str>, usize)> {
    let token = raw.get(index)?.as_str();
    if let Some(long) = token.strip_prefix("--") {
        let (long, inline_value) = long.split_once('=').unwrap_or((long, ""));
        let inline_value = token.contains('=').then_some(inline_value);
        let arg = global_arg_by_long(command, long)?;
        if global_arg_takes_value(arg) {
            if let Some(value) = inline_value {
                return Some((arg, Some(value), 1));
            }
            let value = raw.get(index + 1)?.as_str();
            if value.starts_with('-') {
                return None;
            }
            return Some((arg, Some(value), 2));
        }
        return inline_value.is_none().then_some((arg, None, 1));
    }

    let short_flags = token.strip_prefix('-')?;
    if short_flags.is_empty() {
        return None;
    }

    let chars: Vec<(usize, char)> = short_flags.char_indices().collect();
    let mut offset = 0;
    while offset < chars.len() {
        let (byte_index, short) = chars[offset];
        let arg = global_arg_by_short(command, short)?;
        if global_arg_takes_value(arg) {
            let value_start = byte_index + short.len_utf8();
            if value_start < short_flags.len() {
                return Some((arg, Some(&short_flags[value_start..]), 1));
            }
            let value = raw.get(index + 1)?.as_str();
            if value.starts_with('-') {
                return None;
            }
            return Some((arg, Some(value), 2));
        }
        offset += 1;
    }

    let first_short = chars.first().map(|(_, short)| *short)?;
    Some((global_arg_by_short(command, first_short)?, None, 1))
}

fn global_arg_by_long<'a>(command: &'a clap::Command, long: &str) -> Option<&'a Arg> {
    command
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .find(|arg| {
            arg.get_long() == Some(long)
                || arg
                    .get_all_aliases()
                    .unwrap_or_default()
                    .into_iter()
                    .any(|alias| alias == long)
                || arg
                    .get_visible_aliases()
                    .unwrap_or_default()
                    .into_iter()
                    .any(|alias| alias == long)
        })
}

fn global_arg_by_short(command: &clap::Command, short: char) -> Option<&Arg> {
    command
        .get_arguments()
        .filter(|arg| arg.is_global_set())
        .find(|arg| {
            arg.get_short() == Some(short)
                || arg
                    .get_all_short_aliases()
                    .unwrap_or_default()
                    .into_iter()
                    .any(|alias| alias == short)
                || arg
                    .get_visible_short_aliases()
                    .unwrap_or_default()
                    .into_iter()
                    .any(|alias| alias == short)
        })
}

fn global_arg_takes_value(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Set | ArgAction::Append)
}

fn explicit_json_requested(cli: &Cli) -> bool {
    matches!(
        cli.output_mode(),
        Some(cli::cli::OutputMode::Json | cli::cli::OutputMode::JsonCompact)
    )
}

fn is_broken_pipe_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
        || error.to_string().contains("Broken pipe")
}

fn is_broken_pipe_panic(payload: &(dyn Any + Send)) -> bool {
    payload
        .downcast_ref::<String>()
        .is_some_and(|message| message.contains("Broken pipe"))
        || payload
            .downcast_ref::<&'static str>()
            .is_some_and(|message| message.contains("Broken pipe"))
}

fn install_broken_pipe_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if is_broken_pipe_panic(info.payload()) {
            return;
        }
        previous(info);
    }));
}

/// True for long-running daemon entry points whose stderr is the operator's
/// audit log. These keep an INFO-level default; everything else defaults to
/// WARN so a human running `heddle status` doesn't see internal tracing.
fn is_daemon_invocation(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Daemon {
            command: DaemonCommands::Serve
        } | Commands::Agent {
            command: AgentCommands::Serve(_)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn global_flags_only_accepts_text_output_globals() {
        assert!(is_global_flags_only(&args(&["--output", "text"])));
        assert!(is_global_flags_only(&args(&["--output=text"])));
        assert!(is_global_flags_only(&args(&["--no-color", "-v"])));
        assert!(is_global_flags_only(&args(&["-C", "."])));
        assert!(is_global_flags_only(&args(&["-C."])));
        assert!(is_global_flags_only(&args(&["-vvv"])));
        assert!(is_global_flags_only(&args(&["-qv"])));
    }

    #[test]
    fn global_flags_only_accepts_json_globals() {
        assert!(is_global_flags_only(&args(&["--output", "json"])));
        assert!(is_global_flags_only(&args(&["--output=json"])));
    }

    #[test]
    fn global_flags_only_rejects_commands_unknowns_and_dangling_values() {
        assert!(!is_global_flags_only(&args(&[])));
        assert!(!is_global_flags_only(&args(&["status"])));
        assert!(!is_global_flags_only(&args(&["--not-a-global"])));
        assert!(!is_global_flags_only(&args(&["--output"])));
        assert!(!is_global_flags_only(&args(&["--output", "--no-color"])));
        assert!(!is_global_flags_only(&args(&["--repo"])));
        assert!(!is_global_flags_only(&args(&["-C"])));
    }

    #[test]
    fn raw_wants_json_uses_clap_global_metadata() {
        assert!(raw_wants_json(&args(&["--output", "json"])));
        assert!(raw_wants_json(&args(&["--output=json"])));
        assert!(!raw_wants_json(&args(&["--output", "text"])));
        assert!(!raw_wants_json(&args(&["--output=text"])));
        assert!(!raw_wants_json(&args(&["--output", "--no-color"])));
    }
}
