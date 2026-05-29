// SPDX-License-Identifier: Apache-2.0
//! Heddle: An AI-native version control system.

use std::{any::Any, time::Instant};

use anyhow::Result;
use clap::{Arg, ArgAction, CommandFactory, Parser, error::ErrorKind};
#[cfg(feature = "semantic")]
use cli::cli::commands::cmd_semantic;
#[cfg(feature = "git-overlay")]
use cli::cli::{
    BridgeCommands,
    commands::{cmd_bridge_git, cmd_git_overlay_guide},
};
use cli::{
    cli::{
        ActorCommands, AgentCommands, Cli, CloneArgs, CollapseArgs, Commands, ContextCommands,
        DaemonCommands, DiagnoseArgs, DiffArgs, LogArgs, MergeArgs, ResolveArgs, RetroArgs,
        RevertArgs, RunArgs, SessionCommands, SessionEndArgs, SessionListArgs, SessionSegmentArgs,
        SessionShowArgs, SessionStartArgs, UndoArgs,
        cli_args::{DelegateArgs, ShipArgs, SyncArgs},
        commands::{
            LogCommandOptions, RetroCommandOptions, SnapshotAgentOverrides, build_command_catalog,
            cmd_abort, cmd_actor_done, cmd_actor_explain, cmd_actor_list, cmd_actor_show,
            cmd_actor_spawn, cmd_adopt, cmd_agent, cmd_attempt, cmd_bisect, cmd_blame,
            cmd_branch_compat, cmd_capture_split, cmd_checkpoint, cmd_cherry_pick, cmd_clean,
            cmd_clone, cmd_collapse, cmd_commands, cmd_commit_compat, cmd_compare, cmd_completion,
            cmd_conflict, cmd_context_audit, cmd_context_check, cmd_context_edit, cmd_context_get,
            cmd_context_history, cmd_context_list, cmd_context_rm, cmd_context_set,
            cmd_context_suggest, cmd_context_supersede, cmd_continue, cmd_daemon_serve,
            cmd_daemon_status, cmd_daemon_stop, cmd_delegate, cmd_diagnose, cmd_diff, cmd_discuss,
            cmd_doctor_docs, cmd_doctor_schemas, cmd_fetch, cmd_fork, cmd_fsck, cmd_gc, cmd_goto,
            cmd_harness_bridge, cmd_hook, cmd_index, cmd_init, cmd_integration, cmd_log,
            cmd_maintenance, cmd_marker, cmd_merge, cmd_monitor, cmd_pull, cmd_push, cmd_query,
            cmd_ready, cmd_rebase, cmd_redo, cmd_remote, cmd_resolve, cmd_retro, cmd_revert,
            cmd_review, cmd_run, cmd_schemas, cmd_session_end, cmd_session_list,
            cmd_session_segment, cmd_session_show, cmd_session_start, cmd_shell, cmd_ship,
            cmd_show, cmd_snapshot, cmd_stack, cmd_start, cmd_stash, cmd_status, cmd_store,
            cmd_switch_compat, cmd_sync_smart, cmd_thread, cmd_thread_show, cmd_transaction,
            cmd_try, cmd_undo, cmd_verify, cmd_version, cmd_watch, cmd_workspace,
            command_runtime_contract_for_command, print_error_with_hint,
            print_parse_error_json_envelope,
        },
        render::write_json_stdout,
    },
    config::UserConfig,
    exit::HeddleExitCode,
    logging::{LoggingConfig, init_logging},
    operation_id::{resolve_operation_id, run_local_idempotency_if_requested},
    perf::{ProfileField, emit_profile, profile_enabled},
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
    // any rustls TLS handshake (gRPC, GitHub REST, `bridge git import
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

    // Pick the WeftExtensions implementation at startup. OSS builds
    // get NoopWeftExtensions (returns friendly errors for `auth`,
    // `support`, `presence` commands). client builds get the
    // EnabledWeftExtensions adapter that delegates to the existing
    // in-cli command impls; Step 5 of the OSS extraction plan moves
    // those impls into a separate closed crate.
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
                emit_profile(
                    "help",
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
                emit_profile(
                    "help",
                    &[ProfileField::duration("total_ms", total_start.elapsed())],
                );
            }
            return Ok(());
        }
        // `heddle help <topic>` — let clap handle when the user passes
        // the verb explicitly (it dispatches to Commands::Help). A two-
        // arg form `heddle help <topic>` also goes through clap.
    }
    let cli = match Cli::try_parse() {
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

    if explicit_json_requested(&cli) && !command_contract.supports_json {
        telemetry.shutdown();
        let err = anyhow::anyhow!(cli::cli::commands::RecoveryAdvice::json_unsupported(
            &command_name
        ));
        print_error_with_hint(&cli, &err);
        std::process::exit(HeddleExitCode::Usage.into());
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
            // structured output (help is human-shaped).
            cli::cli::help::print_help(&Cli::command(), topics).map_err(Into::into)
        }

        Commands::Status {
            short,
            watch,
            watch_iterations,
            watch_interval_ms,
        } => cmd_status(&cli, *short, *watch, *watch_iterations, *watch_interval_ms).await,

        Commands::Watch(args) => cmd_watch(&cli, args.clone()).await,

        Commands::Diagnose(DiagnoseArgs { profile }) => {
            cmd_diagnose(&cli, DiagnoseArgs { profile: *profile })
        }

        Commands::Verify => cmd_verify(&cli, cli.verbose > 0),

        Commands::Doctor(args) => match &args.command {
            None => cmd_diagnose(
                &cli,
                DiagnoseArgs {
                    profile: args.profile,
                },
            ),
            Some(cli::cli::DoctorCommands::Docs(docs_args)) => {
                cmd_doctor_docs(&cli, docs_args.clone())
            }
            Some(cli::cli::DoctorCommands::Schemas) => cmd_doctor_schemas(&cli),
        },

        Commands::Schemas { verb } => cmd_schemas(&cli, verb),

        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => cmd_git_overlay_guide(&cli),

        Commands::Version => cmd_version(&cli, cli.verbose > 0),

        Commands::Commands(args) => cmd_commands(&cli, args),

        Commands::Start(args) => cmd_start(&cli, args.clone()),

        Commands::Run(RunArgs { thread, command }) => {
            cmd_run(&cli, thread.clone(), command.clone())
        }

        Commands::Try(args) => cmd_try(&cli, args.clone()),

        Commands::Attempt(args) => cmd_attempt(&cli, args.clone()),

        Commands::Sync(SyncArgs { thread }) => {
            // Codex's enhanced sync (rebase-aware fast-forward path);
            // main wired `cmd_sync` here pre-rebase. The smart variant
            // is a strict superset, so we use it on the merged
            // branch.
            cmd_sync_smart(
                &cli,
                SyncArgs {
                    thread: thread.clone(),
                },
            )
            .await
        }

        Commands::Continue => cmd_continue(&cli).await,

        Commands::Abort => cmd_abort(&cli),

        Commands::Ship(ShipArgs {
            thread,
            message,
            push,
            no_push,
            remote,
        }) => {
            cmd_ship(
                &cli,
                ShipArgs {
                    thread: thread.clone(),
                    message: message.clone(),
                    push: *push,
                    no_push: *no_push,
                    remote: remote.clone(),
                },
            )
            .await
        }

        Commands::Delegate(DelegateArgs {
            tasks,
            parent,
            workspace,
            path_prefix,
            agent_provider,
            agent_model,
        }) => cmd_delegate(
            &cli,
            DelegateArgs {
                tasks: tasks.clone(),
                parent: parent.clone(),
                workspace: *workspace,
                path_prefix: path_prefix.clone(),
                agent_provider: agent_provider.clone(),
                agent_model: agent_model.clone(),
            },
        ),

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

        Commands::Commit(args) => cmd_commit_compat(&cli, args.clone()).await,

        Commands::Log(LogArgs {
            state,
            limit,
            all,
            graph,
            oneline,
            reflog,
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
                    agent: agent.clone(),
                    paths: paths.clone(),
                    since: since.clone(),
                },
            )
            .await
        }

        Commands::Show { state } => cmd_show(&cli, state.clone()),

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

        Commands::Inspect { target } => {
            let cwd;
            let start = if let Some(path) = cli.repo.as_ref() {
                path
            } else {
                cwd = std::env::current_dir()?;
                &cwd
            };
            if let Some(state) = target
                && is_plain_git_without_heddle(start)
            {
                return cmd_show(&cli, Some(state.clone()));
            }
            let repo = repo::Repository::open(start)?;
            match target {
                Some(name) if repo.refs().get_thread(&objects::object::ThreadName::new(name.as_str()))?.is_some() => {
                    cmd_thread_show(&cli, &repo, Some(name.clone()))
                }
                Some(state) => cmd_show(&cli, Some(state.clone())),
                None => cmd_thread_show(&cli, &repo, None),
            }
        }

        Commands::Goto { target, force } => cmd_goto(&cli, target.clone(), *force),

        Commands::Clean { force, dry_run } => cmd_clean(&cli, *force, *dry_run),

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

        Commands::Branch(args) => cmd_branch_compat(&cli, args.clone()).await,

        Commands::Switch(args) | Commands::Checkout(args) => {
            cmd_switch_compat(&cli, args.clone()).await
        }

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
            allow_redact_undo,
        }) => cmd_undo(&cli, *steps, *list, *depth, *preview, *allow_redact_undo),

        Commands::Redo { steps, preview } => cmd_redo(&cli, *steps, *preview),

        Commands::Fetch { remote, all } => cmd_fetch(&cli, remote.clone(), *all).await,

        Commands::Fork { name, from } => cmd_fork(&cli, name.clone(), from.clone()),

        Commands::Fsck {
            full,
            thorough,
            repair,
            bridge,
        } => cmd_fsck(&cli, *full, *thorough, *repair, *bridge),

        Commands::Collapse(CollapseArgs {
            states,
            into,
            confidence,
        }) => cmd_collapse(&cli, states.clone(), into.clone(), *confidence),

        Commands::Compare {
            state_a,
            state_b,
            semantic,
        } => cmd_compare(&cli, state_a.clone(), state_b.clone(), *semantic),

        Commands::Marker { command } => cmd_marker(&cli, command.clone()),

        Commands::Thread { command } => cmd_thread(&cli, command.clone()).await,

        Commands::Shell { command } => cmd_shell(command.clone()),

        Commands::Workspace { command } => cmd_workspace(&cli, command.clone()).await,

        Commands::Stack(args) => cmd_stack(&cli, args.clone()),

        Commands::Merge(MergeArgs {
            thread,
            message,
            no_commit,
            preview,
            with_diff,
            semantic,
            git_commit,
        }) => cmd_merge(
            &cli,
            thread.clone(),
            message.clone(),
            *no_commit,
            *preview,
            *with_diff,
            *semantic,
            *git_commit,
        ),

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
                args.mirror.clone(),
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
        },

        Commands::Integration { command } => cmd_integration(&cli, command.clone()),

        Commands::Stash { command } => cmd_stash(&cli, command.clone()),

        #[cfg(feature = "client")]
        Commands::Support { command } => {
            let cmd = command.clone();
            hosted.support(&cli, &cmd).await
        }

        #[cfg(feature = "git-overlay")]
        Commands::Bridge { command } => match command {
            BridgeCommands::Git { command } => cmd_bridge_git(&cli, command.clone()),
        },

        #[cfg(feature = "semantic")]
        Commands::Semantic { command } => cmd_semantic(&cli, command.clone()),

        Commands::Completion { shell } => cmd_completion(&cli, shell.clone()),

        Commands::Gc {
            prune,
            aggressive,
            dry_run,
        } => cmd_gc(&cli, *prune, *aggressive, *dry_run),

        Commands::Index { dump } => cmd_index(&cli, *dump),

        Commands::Monitor { paths, serve } => cmd_monitor(&cli, *paths, *serve),

        Commands::Daemon { command } => match command {
            DaemonCommands::Serve => cmd_daemon_serve(&cli),
            DaemonCommands::Status => cmd_daemon_status(&cli),
            DaemonCommands::Stop => cmd_daemon_stop(&cli),
        },

        Commands::Agent { command } => cmd_agent(&cli, command).await,

        Commands::Discuss { command } => cmd_discuss(&cli, command).await,

        Commands::Query(args) => cmd_query(&cli, args).await,

        Commands::Checkpoint(args) => cmd_checkpoint(&cli, args).await,

        Commands::Transaction { command } => cmd_transaction(&cli, command).await,

        Commands::Conflict { command } => cmd_conflict(&cli, command).await,

        Commands::Review { command } => cmd_review(&cli, command).await,

        Commands::Redact { command } => cli::cli::commands::cmd_redact(&cli, command.clone()),

        Commands::Purge { command } => cli::cli::commands::cmd_purge(&cli, command.clone()),

        Commands::Maintenance { command } => cmd_maintenance(&cli, command.clone()),

        Commands::Store { command } => cmd_store(&cli, command.clone()),

        Commands::Blame {
            file,
            state,
            context,
        } => cmd_blame(&cli, file.clone(), state.clone(), *context),

        Commands::Bisect { command } => cmd_bisect(&cli, command.clone()),

        Commands::CherryPick {
            commit,
            message,
            no_commit,
            force,
        } => cmd_cherry_pick(&cli, commit.clone(), message.clone(), *no_commit, *force),

        Commands::Clone(CloneArgs {
            remote,
            local,
            thread,
            depth,
            lazy,
            filter,
        }) => {
            cmd_clone(
                &cli,
                remote.clone(),
                local.clone(),
                thread.clone(),
                *depth,
                *lazy,
                filter.clone(),
            )
            .await
        }

        Commands::Rebase {
            thread,
            abort,
            cont,
            force,
        } => cmd_rebase(&cli, thread.as_deref(), *abort, *cont, *force),

        Commands::Hook { command } => cmd_hook(&cli, command.clone()),

        Commands::HarnessBridge => cmd_harness_bridge(&cli),

        Commands::Actor { command } => match command {
            ActorCommands::Spawn(args) => {
                cmd_actor_spawn(
                    &cli,
                    args.thread.clone(),
                    args.provider.clone(),
                    args.model.clone(),
                )
                .await
            }
            ActorCommands::List(args) => cmd_actor_list(&cli, args.active).await,
            ActorCommands::Show(args) => cmd_actor_show(&cli, args.session.clone()).await,
            ActorCommands::Explain(args) => cmd_actor_explain(&cli, args.session.clone()).await,
            ActorCommands::Done(args) => cmd_actor_done(&cli, args.session.clone()).await,
        },

        // cmd_agent is the unified dispatcher: daemon variants
        // (Serve/Status/Stop) plus the reservation API (Reserve/
        // Heartbeat/Capture/Ready/Release/List).
        Commands::Session { command } => match command {
            SessionCommands::Start(SessionStartArgs {
                provider,
                model,
                policy,
            }) => cmd_session_start(&cli, provider.clone(), model.clone(), policy.clone()).await,
            SessionCommands::Segment(SessionSegmentArgs {
                provider,
                model,
                policy,
            }) => cmd_session_segment(&cli, provider.clone(), model.clone(), policy.clone()).await,
            SessionCommands::End(SessionEndArgs { session_id }) => {
                cmd_session_end(&cli, session_id.clone()).await
            }
            SessionCommands::Show(SessionShowArgs { session_id }) => {
                cmd_session_show(&cli, session_id.clone()).await
            }
            SessionCommands::List(SessionListArgs { active }) => {
                cmd_session_list(&cli, *active).await
            }
        },

        #[cfg(feature = "client")]
        Commands::Presence { command } => match command {
            cli::cli::PresenceCommands::Publish {
                session,
                interval_secs,
            } => {
                hosted
                    .presence_publish(&cli, session.clone(), *interval_secs)
                    .await
            }
        },
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
        emit_profile(
            &command_name,
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
            print_error_with_hint(&cli, &err);
            std::process::exit(code.into());
        }
    }
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
        if arg.get_id().as_str() == "output"
            && value.is_some_and(|value| value == "json") {
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
    matches!(cli.output, Some(cli::cli::OutputMode::Json))
}

fn is_plain_git_without_heddle(start: &std::path::Path) -> bool {
    let Ok(git_repo) = gix::discover(start) else {
        return false;
    };
    let Some(workdir) = git_repo.workdir() else {
        return false;
    };
    !workdir.join(".heddle").exists()
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
