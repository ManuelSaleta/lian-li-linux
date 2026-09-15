use clap::{Parser, Subcommand};
use lianli_shared::installation::InstallationContext;

#[derive(Parser)]
#[command(version, about = "Lian Li installation and service management")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect credentials, host-lock and USB node permissions without device I/O.
    DiagnoseRuntime {
        #[arg(long, default_value = "hidraw", value_parser = parse_hid_backend)]
        hid_backend: lianli_shared::config::HidBackend,
    },
    #[command(hide = true)]
    InspectRuntime {
        #[arg(long, value_parser = parse_hid_backend)]
        hid_backend: Option<lianli_shared::config::HidBackend>,
    },
    #[command(hide = true)]
    CheckRecoveryAccess {
        #[arg(long)]
        config: std::path::PathBuf,
    },
    #[command(hide = true)]
    RecoverAutomatic,
    #[command(hide = true)]
    TriggerRecovery,
    #[command(hide = true)]
    SubmitSwitch {
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
        #[command(flatten)]
        change: ChangeArgs,
    },
    #[command(hide = true)]
    RunSwitch {
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
        #[arg(long)]
        caller_uid: u32,
        #[command(flatten)]
        change: ChangeArgs,
    },
    #[command(hide = true)]
    FinishTransfer,
    /// Switch native service mode after administrator authorization
    SwitchMode {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        carry_settings: bool,
    },
    /// Restore the previous mode after an interrupted native service switch
    RecoverSwitch,
    #[command(hide = true)]
    CheckSavedState {
        #[arg(long)]
        config: std::path::PathBuf,
        #[arg(long)]
        working_directory: std::path::PathBuf,
        #[arg(long)]
        decode: bool,
    },
    #[command(hide = true)]
    InspectStartup,
    #[command(hide = true)]
    ObserveService {
        #[arg(long, value_enum)]
        scope: Scope,
    },
    #[command(hide = true)]
    PublishState,
    /// Prepare opposite-mode native settings and media under the destination account
    PrepareTransfer {
        #[arg(long, value_enum)]
        scope: Scope,
    },
    /// Remove only this operation's unpublished preparation under the current account
    DiscardTransfer {
        #[arg(long)]
        config: std::path::PathBuf,
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
    },
    #[command(hide = true)]
    SendState {
        #[arg(long)]
        config: std::path::PathBuf,
        #[arg(long)]
        working_directory: std::path::PathBuf,
        #[arg(long)]
        generation: String,
    },
    #[command(hide = true)]
    SendSelectedMedia {
        #[arg(long)]
        selection: std::path::PathBuf,
    },
    #[command(hide = true)]
    ReceiveSelectedMedia {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
        #[arg(long)]
        expected_config: std::path::PathBuf,
    },
    #[command(hide = true)]
    ImportSelectedMedia {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        selection: std::path::PathBuf,
        #[arg(long)]
        expected_config: std::path::PathBuf,
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
    },
    #[command(hide = true)]
    RunSelectedMediaImport {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        selection: std::path::PathBuf,
        #[arg(long)]
        expected_config: std::path::PathBuf,
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
    },
    #[command(hide = true)]
    ReceiveState {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
        #[arg(long)]
        generation: String,
    },
    #[command(hide = true)]
    RunServiceAction {
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        operation_id: String,
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long, value_enum)]
        action: Action,
    },
    /// Authorize and preflight a native service destination without stopping the current daemon
    CheckDestination {
        #[arg(long, value_enum)]
        scope: Scope,
    },
    /// Inspect the installed native destination under the executing daemon account
    #[command(hide = true)]
    InspectDestination {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        prepare_directory: bool,
    },
    /// Select one native hardware mode while all daemons are stopped (administrator only)
    SelectMode {
        #[arg(long, value_enum)]
        scope: Scope,
        #[arg(long)]
        user_uid: Option<u32>,
    },
    /// Stop only the user daemon belonging to this host service invocation
    StopService {
        #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation)]
        invocation_id: String,
    },
    /// Inspect service setup without starting a daemon or accessing hardware.
    Diagnose,
    /// Inspect saved configuration, profiles, templates and media references without changing them.
    InspectState {
        #[arg(long)]
        config: std::path::PathBuf,
        /// Working directory used by the daemon for relative template assets
        #[arg(long)]
        working_directory: std::path::PathBuf,
        /// Open every saved asset under this account, including inactive profiles and templates
        #[arg(long)]
        check_assets: bool,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum Scope {
    User,
    System,
}

#[derive(Clone, clap::ValueEnum)]
enum Action {
    Start,
    Stop,
    Restart,
}

#[derive(clap::Args)]
struct ChangeArgs {
    #[arg(long, value_enum, required_unless_present = "recover")]
    scope: Option<Scope>,
    #[arg(long, conflicts_with = "scope")]
    recover: bool,
    #[arg(long, requires = "scope", conflicts_with = "recover")]
    carry_settings: bool,
}

impl ChangeArgs {
    fn request(self) -> anyhow::Result<lianli_shared::services::ServiceChangeRequest> {
        use lianli_shared::services::{ServiceChangeRequest, ServiceScope};
        Ok(match (self.scope, self.recover) {
            (None, true) => ServiceChangeRequest::Recover {},
            (Some(scope), false) => ServiceChangeRequest::Switch {
                scope: match scope {
                    Scope::User => ServiceScope::User,
                    Scope::System => ServiceScope::System,
                },
                carry_settings: self.carry_settings,
            },
            _ => anyhow::bail!("Choose one destination mode or recovery"),
        })
    }
}

fn parse_hid_backend(value: &str) -> Result<lianli_shared::config::HidBackend, String> {
    match value {
        "hidraw" => Ok(lianli_shared::config::HidBackend::Hidraw),
        "rusb" => Ok(lianli_shared::config::HidBackend::Rusb),
        _ => Err("Choose hidraw or rusb".into()),
    }
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::InspectRuntime { hid_backend } => println!(
            "{}",
            serde_json::to_string(&lianli_control::runtime_health::inspect(hid_backend)?)?
        ),
        Command::DiagnoseRuntime { hid_backend } => println!(
            "{}",
            serde_json::to_string(&lianli_control::runtime_health::collect_report(
                &std::env::current_exe()?,
                Some(hid_backend)
            )?)?
        ),
        Command::CheckRecoveryAccess { config } => {
            lianli_control::destination::check_recovery_access(&config)?
        }
        Command::RecoverAutomatic => println!("{}", lianli_control::automatic_recovery::run()?),
        Command::TriggerRecovery => lianli_control::automatic_recovery::trigger()?,
        Command::SubmitSwitch {
            operation_id,
            change,
        } => lianli_control::switch_job::submit(&operation_id, change.request()?)?,
        Command::RunSwitch {
            operation_id,
            caller_uid,
            change,
        } => println!(
            "{}",
            lianli_control::switch_job::run(caller_uid, &operation_id, change.request()?)?
        ),
        Command::FinishTransfer => lianli_control::transfer_cleanup::serve()?,
        Command::SwitchMode {
            scope,
            carry_settings,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                lianli_control::native_switch::execute(scope, carry_settings, |message| {
                    eprintln!("{message}");
                    Ok(())
                })?
            );
        }
        Command::RecoverSwitch => println!(
            "{}",
            lianli_control::native_switch::recover(|message| {
                eprintln!("{message}");
                Ok(())
            })?
        ),
        Command::CheckSavedState {
            config,
            working_directory,
            decode,
        } => println!(
            "{}",
            serde_json::to_string(&lianli_control::saved_state::inspect(
                &config,
                &working_directory,
                decode
            )?)?
        ),
        Command::InspectStartup => println!(
            "{}",
            serde_json::to_string(&lianli_control::service_startup::inspect()?)?
        ),
        Command::ObserveService { scope } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string(&lianli_control::authorized_service::observe(scope)?)?
            );
        }
        Command::PublishState => lianli_control::account_publication::serve()?,
        Command::PrepareTransfer { scope } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&lianli_control::state_transfer::prepare_native(
                    scope
                )?)?
            );
        }
        Command::DiscardTransfer {
            config,
            operation_id,
        } => {
            lianli_control::state_transfer::discard(&config, &operation_id)?;
        }
        Command::SendState {
            config,
            working_directory,
            generation,
        } => {
            println!(
                "{}",
                serde_json::to_string(&lianli_control::state_transfer::send(
                    &config,
                    &working_directory,
                    &generation
                )?)?
            );
        }
        Command::SendSelectedMedia { selection } => {
            lianli_control::media_import_transfer::send_file(&selection)?;
        }
        Command::ReceiveSelectedMedia {
            scope,
            operation_id,
            expected_config,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string(&lianli_control::media_import_transfer::receive_published(
                    scope,
                    &operation_id,
                    &expected_config
                )?)?
            );
        }
        Command::ImportSelectedMedia {
            scope,
            selection,
            expected_config,
            operation_id,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string(&lianli_control::media_import_launch::native(
                    scope,
                    &selection,
                    &expected_config,
                    &operation_id
                )?)?
            );
        }
        Command::RunSelectedMediaImport {
            scope,
            selection,
            expected_config,
            operation_id,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            lianli_control::media_import_worker::run(
                scope,
                &selection,
                &expected_config,
                &operation_id,
            )?;
        }
        Command::ReceiveState {
            scope,
            operation_id,
            generation,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string(&lianli_control::state_transfer::receive(
                    scope,
                    &operation_id,
                    &generation
                )?)?
            );
        }
        Command::RunServiceAction {
            operation_id,
            scope,
            action,
        } => {
            use lianli_shared::services::{ServiceAction, ServiceActionRequest, ServiceScope};
            let request = ServiceActionRequest {
                scope: match scope {
                    Scope::User => ServiceScope::User,
                    Scope::System => ServiceScope::System,
                },
                action: match action {
                    Action::Start => ServiceAction::Start,
                    Action::Stop => ServiceAction::Stop,
                    Action::Restart => ServiceAction::Restart,
                },
            };
            println!(
                "{}",
                lianli_control::operation_job::run(&operation_id, request)?
            );
        }
        Command::CheckDestination { scope } => {
            anyhow::ensure!(
                InstallationContext::detect() == InstallationContext::Native,
                "Automatic system/user switching requires the native host application"
            );
            let caller = lianli_control::account::Account::authorized_caller()?;
            let (account, scope) = match scope {
                Scope::User => (caller, lianli_shared::services::ServiceScope::User),
                Scope::System => (
                    lianli_control::account::Account::system()?,
                    lianli_shared::services::ServiceScope::System,
                ),
            };
            let _operation = lianli_control::reservation::ServiceOperationLock::acquire(
                &InstallationContext::Native,
            )?;
            println!(
                "{}",
                serde_json::to_string_pretty(&lianli_control::destination::preflight(
                    &account, scope
                )?)?
            );
        }
        Command::InspectDestination {
            scope,
            prepare_directory,
        } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            println!(
                "{}",
                serde_json::to_string(&lianli_control::destination::inspect(
                    scope,
                    prepare_directory
                )?)?
            );
        }
        Command::SelectMode { scope, user_uid } => {
            let scope = match scope {
                Scope::User => lianli_shared::services::ServiceScope::User,
                Scope::System => lianli_shared::services::ServiceScope::System,
            };
            let selection = lianli_control::service_selection::select(scope, user_uid)?;
            println!("{}", serde_json::to_string_pretty(&selection)?);
        }
        Command::StopService { invocation_id } => {
            lianli_control::service_stop::stop(&InstallationContext::detect(), &invocation_id)?;
            println!("The service daemon exited.");
        }
        Command::Diagnose => {
            let context = InstallationContext::detect();
            let report = lianli_control::services::inspect(&context);
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::InspectState {
            config,
            working_directory,
            check_assets,
        } => {
            let snapshot = lianli_control::state::StateSnapshot::read(&config, &working_directory)?;
            if check_assets {
                let access =
                    snapshot.check_assets(&lianli_control::media_staging::CopyControl::new(
                        std::time::Duration::from_secs(30),
                    ))?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "state": snapshot.summary, "assets": access
                    }))?
                );
                anyhow::ensure!(
                    snapshot.summary.issue_count == 0 && access.failed == 0,
                    "Saved state or asset access failed validation; see the JSON report"
                );
            } else {
                println!("{}", serde_json::to_string_pretty(&snapshot.summary)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod switch_cli_tests {
    use super::*;

    #[test]
    fn switch_submission_accepts_one_mode_or_recovery_without_executing_it() {
        let base = [
            "lianli-control",
            "submit-switch",
            "--operation-id",
            "0123456789abcdef0123456789abcdef",
        ];
        for args in [
            &["--scope", "user"][..],
            &["--scope", "system", "--carry-settings"],
            &["--recover"],
        ] {
            assert!(Cli::try_parse_from(base.into_iter().chain(args.iter().copied())).is_ok());
        }
        for args in [
            &[][..],
            &["--scope", "root"],
            &["--recover", "--carry-settings"],
            &["--recover", "--scope", "user"],
        ] {
            assert!(
                Cli::try_parse_from(base.into_iter().chain(args.iter().copied())).is_err(),
                "Accepted invalid arguments: {args:?}"
            );
        }
    }
}
