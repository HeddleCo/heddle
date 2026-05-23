// SPDX-License-Identifier: Apache-2.0
//! Heddle: An AI-native version control system.

use std::time::Instant;

use anyhow::Result;
use clap::{CommandFactory, Parser};
#[cfg(feature = "semantic")]
use cli::cli::commands::cmd_semantic;
use cli::{
    cli::{
        ActorCommands, AgentCommands, Cli, CloneArgs, CollapseArgs, Commands,
        ContextCommands, DaemonCommands, DiagnoseArgs, DiffArgs, LogArgs, MergeArgs, ResolveArgs,
        RetroArgs, RevertArgs, RunArgs, SessionCommands, SessionEndArgs, SessionListArgs,
        SessionSegmentArgs, SessionShowArgs, SessionStartArgs, UndoArgs,
        cli_args::{DelegateArgs, ShipArgs, SyncArgs},
        commands::{
            LogCommandOptions, RetroCommandOptions, SnapshotAgentOverrides, cmd_abort,
            cmd_actor_done, cmd_actor_explain, cmd_actor_list, cmd_actor_show, cmd_actor_spawn,
            cmd_agent, cmd_attempt, cmd_bisect, cmd_blame,
            cmd_capture_split, cmd_checkpoint, cmd_cherry_pick, cmd_clean, cmd_clone, cmd_collapse,
            cmd_compare, cmd_completion, cmd_conflict, cmd_context_audit, cmd_context_check,
            cmd_context_edit, cmd_context_get, cmd_context_history, cmd_context_list,
            cmd_context_rm, cmd_context_set, cmd_context_suggest, cmd_context_supersede,
            cmd_continue, cmd_daemon_serve, cmd_daemon_status, cmd_daemon_stop, cmd_delegate,
            cmd_diagnose, cmd_diff, cmd_discuss, cmd_doctor_docs, cmd_doctor_schemas, cmd_fetch,
            cmd_fork, cmd_fsck, cmd_gc, cmd_goto, cmd_harness_bridge,
            cmd_hook, cmd_index, cmd_init, cmd_integration, cmd_log, cmd_maintenance, cmd_marker,
            cmd_merge, cmd_monitor, cmd_pull, cmd_push, cmd_query, cmd_ready, cmd_rebase, cmd_redo,
            cmd_remote, cmd_resolve, cmd_retro, cmd_revert, cmd_review, cmd_run, cmd_schemas,
            cmd_session_end, cmd_session_list, cmd_session_segment, cmd_session_show,
            cmd_session_start, cmd_shell, cmd_ship, cmd_show, cmd_snapshot, cmd_stack, cmd_start,
            cmd_stash, cmd_status, cmd_store, cmd_sync_smart, cmd_thread, cmd_thread_show,
            cmd_transaction,
            cmd_try, cmd_undo, cmd_version, cmd_watch, cmd_workspace,
        },
    },
    config::UserConfig,
    logging::{LoggingConfig, init_logging},
    operation_id::resolve_operation_id,
};
#[cfg(feature = "git-overlay")]
use cli::cli::{BridgeCommands, commands::{cmd_bridge_git, cmd_git_overlay_guide}};
use tracing::debug;

// `current_thread` flavor avoids spinning up a CPU-count-sized worker
// pool on every CLI invocation. The foreground `heddle` binary is a
// one-shot command — `heddle status`, `heddle capture`, etc. don't
// fan out across cores. Daemon variants (`heddle daemon serve`,
// `heddle agent serve`) override this with their own runtime setup
// when they need real concurrency. Saves ~10-30ms of startup that the
// multi-thread flavor pays for thread-pool creation + teardown.
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
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
            cli::cli::help::print_help(&Cli::command(), None)?;
            return Ok(());
        }
        // `heddle help <topic>` — let clap handle when the user passes
        // the verb explicitly (it dispatches to Commands::Help). A two-
        // arg form `heddle help <topic>` also goes through clap.
    }
    let cli = Cli::parse();
    // Resolve color decision once, before any rendering site fires.
    // The helpers in `cli::style` consult a process-wide OnceLock —
    // doing this inside each render path would re-query the env on
    // every line and fight the brand goal of restraint.
    cli::cli::style::init_from_cli(&cli);
    let command_name = command_name(&cli.command);
    let config_start = Instant::now();
    let user_config = UserConfig::load_default()?;
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
        command = command_name,
        config_load_ms,
        logging_init_ms,
        startup_ms = total_start.elapsed().as_millis(),
        "CLI startup complete"
    );

    let command_start = Instant::now();
    let result = match &cli.command {
        Commands::Init(args) => {
            resolve_operation_id(&cli)?;
            cmd_init(&cli, args.clone())
        }

        Commands::Help { topic } => {
            // Curated help printer. No op-id (read-only), no
            // structured output (help is human-shaped).
            cli::cli::help::print_help(&Cli::command(), topic.as_deref()).map_err(Into::into)
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

        Commands::Schemas { verb } => {
            let joined = verb.join(" ");
            cmd_schemas(&cli, &joined)
        }

        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => cmd_git_overlay_guide(&cli),

        Commands::Version => cmd_version(&cli, cli.verbose > 0),

        Commands::Start(args) => {
            resolve_operation_id(&cli)?;
            cmd_start(&cli, args.clone())
        }

        Commands::Run(RunArgs { thread, command }) => {
            resolve_operation_id(&cli)?;
            cmd_run(&cli, thread.clone(), command.clone())
        }

        Commands::Try(args) => {
            resolve_operation_id(&cli)?;
            cmd_try(&cli, args.clone())
        }

        Commands::Attempt(args) => {
            resolve_operation_id(&cli)?;
            cmd_attempt(&cli, args.clone())
        }

        Commands::Sync(SyncArgs { thread }) => {
            resolve_operation_id(&cli)?;
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

        Commands::Continue => {
            resolve_operation_id(&cli)?;
            cmd_continue(&cli).await
        }

        Commands::Abort => {
            resolve_operation_id(&cli)?;
            cmd_abort(&cli)
        }

        Commands::Ship(ShipArgs {
            thread,
            message,
            push,
            no_push,
            remote,
        }) => {
            resolve_operation_id(&cli)?;
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
        }) => {
            resolve_operation_id(&cli)?;
            cmd_delegate(
                &cli,
                DelegateArgs {
                    tasks: tasks.clone(),
                    parent: parent.clone(),
                    workspace: *workspace,
                    path_prefix: path_prefix.clone(),
                    agent_provider: agent_provider.clone(),
                    agent_model: agent_model.clone(),
                },
            )
        }

        Commands::Ready(args) => {
            resolve_operation_id(&cli)?;
            cmd_ready(&cli, args.clone()).await
        }

        Commands::Capture(args) => {
            resolve_operation_id(&cli)?;
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

        // The Checkpoint arm lives further down (alongside main's
        // other write commands) so it picks up resolve_operation_id
        // for telemetry.
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
            let repo =
                repo::Repository::open(cli.repo.as_ref().unwrap_or(&std::env::current_dir()?))?;
            match target {
                Some(name) if repo.refs().get_thread(name)?.is_some() => {
                    cmd_thread_show(&cli, &repo, Some(name.clone()))
                }
                Some(state) => cmd_show(&cli, state.clone()),
                None => cmd_thread_show(&cli, &repo, None),
            }
        }

        Commands::Goto { target, force } => {
            resolve_operation_id(&cli)?;
            cmd_goto(&cli, target.clone(), *force)
        }

        Commands::Clean { force, dry_run } => {
            resolve_operation_id(&cli)?;
            cmd_clean(&cli, *force, *dry_run)
        }

        Commands::Diff(DiffArgs {
            from,
            to,
            semantic,
            stat,
            name_only,
            unified,
            context,
        }) => cmd_diff(
            &cli,
            from.clone(),
            to.clone(),
            *semantic,
            *stat,
            *name_only,
            *unified,
            *context,
        ),

        Commands::Revert(RevertArgs {
            state,
            message,
            no_commit,
        }) => {
            resolve_operation_id(&cli)?;
            cmd_revert(&cli, state.clone(), message.clone(), *no_commit)
        }

        Commands::Undo(UndoArgs {
            steps,
            list,
            depth,
            preview,
            allow_redact_undo,
        }) => {
            resolve_operation_id(&cli)?;
            cmd_undo(&cli, *steps, *list, *depth, *preview, *allow_redact_undo)
        }

        Commands::Redo { steps, preview } => {
            resolve_operation_id(&cli)?;
            cmd_redo(&cli, *steps, *preview)
        }

        Commands::Fetch { remote, all } => {
            resolve_operation_id(&cli)?;
            cmd_fetch(&cli, remote.clone(), *all).await
        }

        Commands::Fork { name, from } => {
            resolve_operation_id(&cli)?;
            cmd_fork(&cli, name.clone(), from.clone())
        }

        Commands::Fsck {
            full,
            thorough,
            repair,
            bridge,
        } => {
            resolve_operation_id(&cli)?;
            cmd_fsck(&cli, *full, *thorough, *repair, *bridge)
        }

        Commands::Collapse(CollapseArgs {
            states,
            into,
            confidence,
        }) => {
            resolve_operation_id(&cli)?;
            cmd_collapse(&cli, states.clone(), into.clone(), *confidence)
        }

        Commands::Compare {
            state_a,
            state_b,
            semantic,
        } => cmd_compare(&cli, state_a.clone(), state_b.clone(), *semantic),

        Commands::Marker { command } => {
            resolve_operation_id(&cli)?;
            cmd_marker(&cli, command.clone())
        }

        Commands::Thread { command } => {
            resolve_operation_id(&cli)?;
            cmd_thread(&cli, command.clone()).await
        }

        Commands::Shell { command } => cmd_shell(command.clone()),

        Commands::Workspace { command } => {
            resolve_operation_id(&cli)?;
            cmd_workspace(&cli, command.clone()).await
        }

        Commands::Stack(args) => {
            resolve_operation_id(&cli)?;
            cmd_stack(&cli, args.clone())
        }

        Commands::Merge(MergeArgs {
            thread,
            message,
            no_commit,
            preview,
            with_diff,
            semantic,
            git_commit,
        }) => {
            resolve_operation_id(&cli)?;
            cmd_merge(
                &cli,
                thread.clone(),
                message.clone(),
                *no_commit,
                *preview,
                *with_diff,
                *semantic,
                *git_commit,
            )
        }

        Commands::Resolve(ResolveArgs {
            path,
            all,
            list,
            ours,
            theirs,
            abort,
        }) => {
            resolve_operation_id(&cli)?;
            cmd_resolve(&cli, path.clone(), *all, *list, *ours, *theirs, *abort)
        }

        Commands::Push(args) => {
            resolve_operation_id(&cli)?;
            cmd_push(
                &cli,
                args.remote_op.remote.clone(),
                args.remote_op.thread.clone(),
                args.state.clone(),
                args.force,
            )
            .await
        }

        Commands::Pull(args) => {
            resolve_operation_id(&cli)?;
            cmd_pull(
                &cli,
                args.remote_op.remote.clone(),
                args.remote_op.thread.clone(),
                args.local_thread.clone(),
                args.lazy,
            )
            .await
        }

        Commands::Remote { command } => {
            resolve_operation_id(&cli)?;
            cmd_remote(&cli, command.clone())
        }

        #[cfg(feature = "client")]
        Commands::Auth { command } => {
            resolve_operation_id(&cli)?;
            let cmd = command.clone();
            hosted.auth(&cli, &cmd).await
        }

        Commands::Context { command } => {
            resolve_operation_id(&cli)?;
            match command {
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
            }
        }

        Commands::Integration { command } => {
            resolve_operation_id(&cli)?;
            cmd_integration(&cli, command.clone())
        }

        Commands::Stash { command } => {
            resolve_operation_id(&cli)?;
            cmd_stash(&cli, command.clone())
        }

        #[cfg(feature = "client")]
        Commands::Support { command } => {
            resolve_operation_id(&cli)?;
            let cmd = command.clone();
            hosted.support(&cli, &cmd).await
        }

        #[cfg(feature = "git-overlay")]
        Commands::Bridge { command } => {
            resolve_operation_id(&cli)?;
            match command {
                BridgeCommands::Git { command } => cmd_bridge_git(&cli, command.clone()),
            }
        }

        #[cfg(feature = "semantic")]
        Commands::Semantic { command } => cmd_semantic(&cli, command.clone()),

        Commands::Completion { shell } => cmd_completion(&cli, shell.clone()),

        Commands::Gc {
            prune,
            aggressive,
            dry_run,
        } => {
            resolve_operation_id(&cli)?;
            cmd_gc(&cli, *prune, *aggressive, *dry_run)
        }

        Commands::Index { dump } => cmd_index(&cli, *dump),

        Commands::Monitor { paths, serve } => cmd_monitor(&cli, *paths, *serve),

        Commands::Daemon { command } => {
            resolve_operation_id(&cli)?;
            match command {
                DaemonCommands::Serve => cmd_daemon_serve(&cli),
                DaemonCommands::Status => cmd_daemon_status(&cli),
                DaemonCommands::Stop => cmd_daemon_stop(&cli),
            }
        }

        Commands::Agent { command } => {
            resolve_operation_id(&cli)?;
            cmd_agent(&cli, command).await
        }

        Commands::Discuss { command } => {
            resolve_operation_id(&cli)?;
            cmd_discuss(&cli, command).await
        }

        Commands::Query(args) => cmd_query(&cli, args).await,

        Commands::Checkpoint(args) => {
            resolve_operation_id(&cli)?;
            cmd_checkpoint(&cli, args).await
        }

        Commands::Transaction { command } => {
            resolve_operation_id(&cli)?;
            cmd_transaction(&cli, command).await
        }

        Commands::Conflict { command } => {
            resolve_operation_id(&cli)?;
            cmd_conflict(&cli, command).await
        }

        Commands::Review { command } => {
            resolve_operation_id(&cli)?;
            cmd_review(&cli, command).await
        }

        Commands::Redact { command } => {
            resolve_operation_id(&cli)?;
            cli::cli::commands::cmd_redact(&cli, command.clone())
        }

        Commands::Purge { command } => {
            resolve_operation_id(&cli)?;
            cli::cli::commands::cmd_purge(&cli, command.clone())
        }

        Commands::Maintenance { command } => {
            resolve_operation_id(&cli)?;
            cmd_maintenance(&cli, command.clone())
        }

        Commands::Store { command } => {
            resolve_operation_id(&cli)?;
            cmd_store(&cli, command.clone())
        }

        Commands::Blame {
            file,
            state,
            context,
        } => cmd_blame(&cli, file.clone(), state.clone(), *context),

        Commands::Bisect { command } => {
            resolve_operation_id(&cli)?;
            cmd_bisect(&cli, command.clone())
        }

        Commands::CherryPick {
            commit,
            message,
            no_commit,
            force,
        } => {
            resolve_operation_id(&cli)?;
            cmd_cherry_pick(&cli, commit.clone(), message.clone(), *no_commit, *force)
        }

        Commands::Clone(CloneArgs {
            remote,
            local,
            thread,
            depth,
            lazy,
            filter,
        }) => {
            resolve_operation_id(&cli)?;
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
        } => {
            resolve_operation_id(&cli)?;
            cmd_rebase(&cli, thread.as_deref(), *abort, *cont, *force)
        }

        Commands::Hook { command } => {
            resolve_operation_id(&cli)?;
            cmd_hook(&cli, command.clone())
        }

        Commands::HarnessBridge => cmd_harness_bridge(&cli),

        Commands::Actor { command } => {
            resolve_operation_id(&cli)?;
            match command {
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
            }
        }

        // The Agent arm lives further up (alongside the other write
        // commands) so it picks up resolve_operation_id for telemetry.
        // cmd_agent is the unified dispatcher: daemon variants
        // (Serve/Status/Stop) plus the reservation API (Reserve/
        // Heartbeat/Capture/Ready/Release/List).
        Commands::Session { command } => {
            resolve_operation_id(&cli)?;
            match command {
                SessionCommands::Start(SessionStartArgs {
                    provider,
                    model,
                    policy,
                }) => {
                    cmd_session_start(&cli, provider.clone(), model.clone(), policy.clone()).await
                }
                SessionCommands::Segment(SessionSegmentArgs {
                    provider,
                    model,
                    policy,
                }) => {
                    cmd_session_segment(&cli, provider.clone(), model.clone(), policy.clone()).await
                }
                SessionCommands::End(SessionEndArgs { session_id }) => {
                    cmd_session_end(&cli, session_id.clone()).await
                }
                SessionCommands::Show(SessionShowArgs { session_id }) => {
                    cmd_session_show(&cli, session_id.clone()).await
                }
                SessionCommands::List(SessionListArgs { active }) => {
                    cmd_session_list(&cli, *active).await
                }
            }
        }

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
        command = command_name,
        config_load_ms,
        logging_init_ms,
        command_body_ms = command_start.elapsed().as_millis(),
        total_ms = total_start.elapsed().as_millis(),
        "CLI command complete"
    );

    telemetry.shutdown();
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            print_error_with_hint(&cli, &err);
            std::process::exit(1);
        }
    }
}

/// Print an error to stderr with a one-line next-step hint when the error
/// chain matches a known recoverable condition. Stays out of the way
/// otherwise — `anyhow`'s `Debug` impl is good enough for arbitrary errors.
///
/// Honours the resolved output format: when JSON is selected, emits a
/// single-line `{"error": …, "hint": …, "kind": …}` envelope instead of
/// freeform text so scripts can parse it cleanly. The envelope is a
/// stderr-only contract — the 21 stdout schemas in
/// `crates/cli/src/cli/commands/schemas.rs` are untouched.
fn print_error_with_hint(cli: &Cli, err: &anyhow::Error) {
    let (hint, kind) = classify_error(err);
    let json = cli::cli::should_output_json(cli, None);
    if json {
        let body = serde_json::json!({
            "error": format!("{err:#}"),
            "hint": hint.unwrap_or_default(),
            "kind": kind.unwrap_or_default(),
        });
        eprintln!("{body}");
    } else {
        eprintln!("Error: {err:#}");
        if let Some(hint) = hint {
            eprintln!("Hint: {hint}");
        }
    }
}

/// Match the error chain against the `HeddleError` variants and named
/// `objects::fs_atomic` predicates we promise actionable hints for. Returns
/// `(hint, kind)` for the matched class, or `(None, None)` when no specific
/// guidance applies.
fn classify_error(err: &anyhow::Error) -> (Option<&'static str>, Option<&'static str>) {
    use objects::error::HeddleError;
    for cause in err.chain() {
        if let Some(heddle_err) = cause.downcast_ref::<HeddleError>() {
            match heddle_err {
                HeddleError::RepositoryNotFound(_) => {
                    return (
                        Some("Run `heddle init` to initialize a repository here."),
                        Some("repository_not_found"),
                    );
                }
                HeddleError::RepositoryExists(_) => {
                    return (
                        Some("Run `heddle status` to inspect the existing repository."),
                        Some("repository_exists"),
                    );
                }
                HeddleError::StateNotFound(_) => {
                    return (
                        Some("List recent states with `heddle log`."),
                        Some("state_not_found"),
                    );
                }
                HeddleError::Io(io) => {
                    if objects::fs_atomic::is_out_of_space(io) {
                        return (Some("Free disk space and retry."), Some("out_of_space"));
                    }
                    if objects::fs_atomic::is_permission_denied(io) {
                        return (
                            Some("Check filesystem permissions on the repository directory."),
                            Some("permission_denied"),
                        );
                    }
                    if objects::fs_atomic::is_read_only_filesystem(io) {
                        return (
                            Some(
                                "Remount the filesystem read-write or move the repo to a writable path.",
                            ),
                            Some("read_only_filesystem"),
                        );
                    }
                }
                _ => {}
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            if objects::fs_atomic::is_out_of_space(io) {
                return (Some("Free disk space and retry."), Some("out_of_space"));
            }
            if objects::fs_atomic::is_permission_denied(io) {
                return (
                    Some("Check filesystem permissions on the repository directory."),
                    Some("permission_denied"),
                );
            }
        }
    }
    // Fallback: string-shape matching for anyhow-only errors that don't carry
    // a downcastable `HeddleError` variant. The matches here are narrow on
    // purpose (anchored to the top of the displayed message), so they only
    // fire for the exact phrasings the CLI itself produces.
    let top = format!("{err:#}");
    if top.starts_with("State not found:") {
        return (
            Some("List recent states with `heddle log`."),
            Some("state_not_found"),
        );
    }
    if top.starts_with("Thread not found:") {
        return (
            Some("List threads with `heddle thread list`."),
            Some("thread_not_found"),
        );
    }
    (None, None)
}

/// True when the raw argv (after the program name) contains only global
/// flags and their values — i.e. the user typed `heddle --output text` or
/// `heddle --no-color -v` with no subcommand verb. We want to show the
/// curated everyday-verb help in that case, not clap's wall-of-subcommands
/// error.
///
/// The list of global flags must stay in lockstep with the `#[arg(...,
/// global = true)]` attributes in [`cli::cli::cli_args::Cli`]. Adding a new
/// global flag without updating this function means typing the new flag
/// alone falls through to clap's noisy error path.
fn is_global_flags_only(raw: &[String]) -> bool {
    if raw.is_empty() {
        return false; // caller already handles the truly-empty case
    }
    let mut iter = raw.iter().peekable();
    while let Some(arg) = iter.next() {
        // Inline `--flag=value` forms — accept the whole token.
        let inline = arg.starts_with("--")
            && arg.contains('=')
            && matches!(
                arg.split_once('=').map(|(k, _)| k),
                Some("--output" | "--repo" | "--op-id"),
            );
        if inline {
            continue;
        }
        match arg.as_str() {
            // No-value global flags (incl. clustered `-vv`, `-vvv`).
            "--json" | "--no-color" | "-q" | "--quiet" => continue,
            "-v" | "--verbose" => continue,
            s if s.starts_with("-v") && s.len() > 2 && s[1..].chars().all(|c| c == 'v') => {
                continue;
            }
            // Value-taking global flags — consume the following token.
            "--output" | "--repo" | "--op-id" => {
                if iter.next().is_none() {
                    // Dangling `--output` with no value — fall through to
                    // clap so the user sees the real parse error.
                    return false;
                }
                continue;
            }
            _ => return false,
        }
    }
    true
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

fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Init(_) => "init",
        Commands::Help { .. } => "help",
        Commands::Status { .. } => "status",
        Commands::Watch(_) => "watch",
        Commands::Diagnose(_) => "diagnose",
        Commands::Doctor(_) => "doctor",
        Commands::Schemas { .. } => "schemas",
        #[cfg(feature = "git-overlay")]
        Commands::GitOverlay => "git-overlay",
        Commands::Version => "version",
        Commands::Start(_) => "start",
        Commands::Run(_) => "run",
        Commands::Try(_) => "try",
        Commands::Attempt(_) => "attempt",
        Commands::Sync(_) => "sync",
        Commands::Continue => "continue",
        Commands::Abort => "abort",
        Commands::Ship(_) => "ship",
        Commands::Delegate(_) => "delegate",
        Commands::Ready(_) => "ready",
        Commands::Capture(_) => "capture",
        Commands::Checkpoint(_) => "checkpoint",
        Commands::Log(_) => "log",
        Commands::Show { .. } => "show",
        Commands::Retro(_) => "retro",
        Commands::Inspect { .. } => "inspect",
        Commands::Goto { .. } => "goto",
        Commands::Clean { .. } => "clean",
        Commands::Diff(_) => "diff",
        Commands::Revert(_) => "revert",
        Commands::Undo(_) => "undo",
        Commands::Redo { .. } => "redo",
        Commands::Fork { .. } => "fork",
        Commands::Collapse(_) => "collapse",
        Commands::Compare { .. } => "compare",
        Commands::Marker { .. } => "marker",
        Commands::Thread { .. } => "thread",
        Commands::Shell { .. } => "shell",
        Commands::Workspace { .. } => "workspace",
        Commands::Stack(_) => "stack",
        Commands::Merge(_) => "merge",
        Commands::Resolve(_) => "resolve",
        Commands::Fsck { .. } => "fsck",
        Commands::Fetch { .. } => "fetch",
        Commands::Push(_) => "push",
        Commands::Pull(_) => "pull",
        Commands::Remote { .. } => "remote",
        #[cfg(feature = "client")]
        Commands::Auth { .. } => "auth",
        Commands::Context { .. } => "context",
        Commands::Integration { .. } => "integration",
        Commands::Stash { .. } => "stash",
        #[cfg(feature = "client")]
        Commands::Support { .. } => "support",
        #[cfg(feature = "git-overlay")]
        Commands::Bridge { .. } => "bridge",
        #[cfg(feature = "semantic")]
        Commands::Semantic { .. } => "semantic",
        Commands::Completion { .. } => "completion",
        Commands::Gc { .. } => "gc",
        Commands::Index { .. } => "index",
        Commands::Monitor { .. } => "monitor",
        Commands::Daemon { .. } => "daemon",
        Commands::Agent { .. } => "agent",
        Commands::Discuss { .. } => "discuss",
        Commands::Query(_) => "query",
        Commands::Transaction { .. } => "transaction",
        Commands::Conflict { .. } => "conflict",
        Commands::Review { .. } => "review",
        Commands::Redact { .. } => "redact",
        Commands::Purge { .. } => "purge",
        Commands::Maintenance { .. } => "maintenance",
        Commands::Store { .. } => "store",
        Commands::Blame { .. } => "blame",
        Commands::Bisect { .. } => "bisect",
        Commands::CherryPick { .. } => "cherry-pick",
        Commands::Clone(_) => "clone",
        Commands::Rebase { .. } => "rebase",
        Commands::Hook { .. } => "hook",
        Commands::HarnessBridge => "harness-bridge",
        Commands::Actor { .. } => "actor",
        Commands::Session { .. } => "session",
        #[cfg(feature = "client")]
        Commands::Presence { .. } => "presence",
    }
}