use clap::{Parser, Subcommand};
use codex_handoff::{
    App, AppPaths, AppServerProbe, AppServerUsageReader, CodexExecHiRunner, ProfileName,
    SystemCodexRunner, SystemLoginRunner, SystemProcessGuard,
};
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::{ExitCode, ExitStatus},
    time::Duration,
};

mod presentation;

#[derive(Parser)]
#[command(
    name = "ch",
    version,
    about = "Safely hand off local Codex auth.json profiles"
)]
struct Cli {
    /// Emit machine-readable JSON for supported read commands.
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, env = "CODEX_HOME")]
    codex_home: Option<PathBuf>,
    #[arg(long, env = "CODEX_HANDOFF_HOME")]
    handoff_home: Option<PathBuf>,
    #[arg(long, env = "CODEX_HANDOFF_CODEX_BIN", default_value = "codex")]
    codex_bin: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save the currently authenticated Codex account as the first profile.
    Init,
    /// Sign in through Codex and save a profile without changing the active one.
    Add {
        /// Use this profile name instead of deriving it from the account email.
        #[arg(long)]
        name: Option<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// Refresh a saved profile through the official Codex login flow.
    Relogin {
        name: String,
        #[arg(short, long)]
        force: bool,
    },
    /// Rename a saved profile without changing its authentication.
    Rename { current: String, new_name: String },
    /// Permanently remove a non-active, idle profile.
    Remove {
        name: String,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Atomically activate a saved profile.
    Switch {
        name: String,
        #[arg(short, long, conflicts_with = "force")]
        close_clients: bool,
        #[arg(short, long, conflicts_with = "close_clients")]
        force: bool,
    },
    /// Persist the current live authentication into its active profile.
    Sync {
        #[arg(short, long)]
        force: bool,
    },
    /// List saved profiles and optionally query quota.
    List {
        /// Do not start Codex or query remote quota.
        #[arg(long)]
        offline: bool,
        /// Maximum simultaneous quota queries.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16))]
        concurrency: u8,
    },
    /// Show the active profile and its live quota.
    Status,
    /// Check local integrity and app-server protocol compatibility.
    Doctor,
    /// Recommend the healthiest profile with the most remaining quota.
    Best {
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16))]
        concurrency: u8,
    },
    /// Send a prompt independently through every healthy profile.
    Hi {
        #[arg(default_value = "hi")]
        prompt: String,
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=16))]
        concurrency: u8,
        /// Per-profile deadline in seconds.
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
        timeout: u64,
    },
    /// Run Codex in an isolated profile home; use `best` for a recommendation.
    Run {
        name: String,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            eprintln!("ch: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let default_paths = AppPaths::from_environment()?;
    let paths = AppPaths::new(
        cli.codex_home
            .unwrap_or_else(|| default_paths.codex_home().to_path_buf()),
        cli.handoff_home
            .unwrap_or_else(|| default_paths.handoff_home().to_path_buf()),
    );
    let app = App::with_all_components(
        paths,
        Box::new(AppServerProbe::from_path(cli.codex_bin.clone())),
        Box::new(SystemProcessGuard),
        Box::new(SystemLoginRunner::from_path(cli.codex_bin.clone())),
    )
    .with_usage_reader(Box::new(AppServerUsageReader::from_path(
        cli.codex_bin.clone(),
    )))
    .with_hi_runner(Box::new(CodexExecHiRunner::from_path(
        cli.codex_bin.clone(),
    )))
    .with_codex_runner(Box::new(SystemCodexRunner::from_path(cli.codex_bin)));
    match cli.command {
        Command::Init => {
            app.init()?;
            println!("saved the current Codex authentication as the active profile");
        }
        Command::Add { name, force } => {
            let name = match name {
                Some(name) => app.add_named(ProfileName::parse(name)?, force)?,
                None => app.add(force)?,
            };
            println!(
                "saved profile `{}`; the previous profile remains active",
                name.as_str()
            );
        }
        Command::Relogin { name, force } => {
            app.relogin(ProfileName::parse(name)?, force)?;
            println!("updated the profile; the previous profile remains active");
        }
        Command::Rename { current, new_name } => {
            app.rename_profile(&ProfileName::parse(current)?, ProfileName::parse(new_name)?)?;
            println!("renamed profile");
        }
        Command::Remove { name, yes } => {
            let name = ProfileName::parse(name)?;
            if !yes && !confirm_removal(&name)? {
                println!("profile was not removed");
                return Ok(ExitCode::SUCCESS);
            }
            app.remove_profile(&name)?;
            println!("removed profile `{}`", name.as_str());
        }
        Command::Switch {
            name,
            force,
            close_clients,
        } => {
            app.switch_with_options(ProfileName::parse(name)?, force, close_clients)?;
            println!("switched profile; start a new Codex session");
        }
        Command::Sync { force } => {
            app.sync(force)?;
            println!("saved the latest live authentication");
        }
        Command::List {
            offline,
            concurrency,
        } => {
            let entries = app.list_with_concurrency(!offline, usize::from(concurrency))?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "profiles": entries,
                    }))?
                );
                return Ok(ExitCode::SUCCESS);
            }
            let output = presentation::render_list(&entries);
            if output.is_empty() {
                println!("No profiles saved. Run `ch init` first.");
            } else {
                println!("{output}");
            }
        }
        Command::Status => {
            let status = app.status()?;
            let usage = app.current_live_usage()?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "active": status.active,
                        "usage": usage,
                        "live_auth_path": app.paths().live_auth_path(),
                        "vault_path": app.paths().handoff_home(),
                    }))?
                );
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "{}",
                presentation::render_status(
                    &status.active,
                    &usage,
                    &app.paths().live_auth_path(),
                    app.paths().handoff_home(),
                )
            );
        }
        Command::Doctor => {
            let checks = app.doctor_checks();
            let failed = checks
                .iter()
                .any(|check| check.status == codex_handoff::DoctorStatus::Fail);
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "checks": checks,
                    }))?
                );
            } else {
                for check in &checks {
                    println!("{}: {}", check.label, check.message);
                }
            }
            return Ok(if failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            });
        }
        Command::Best { concurrency } => {
            let recommendation = app.best(usize::from(concurrency))?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "schema_version": 1,
                        "recommendation": recommendation,
                    }))?
                );
            } else if let Some(profile) = &recommendation.profile {
                println!("{}", profile.as_str());
            } else {
                print_ineligible(&recommendation);
            }
            return Ok(if recommendation.profile.is_some() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            });
        }
        Command::Hi {
            prompt,
            concurrency,
            timeout,
        } => {
            let results = app.hi_with_options(
                &prompt,
                usize::from(concurrency),
                Duration::from_secs(timeout),
            )?;
            println!("{}", presentation::render_hi_results(&results, &prompt));
        }
        Command::Run { name, args } => {
            let name = if name == "best" {
                let recommendation = app.best(4)?;
                let Some(name) = recommendation.profile else {
                    print_ineligible(&recommendation);
                    return Ok(ExitCode::from(2));
                };
                name
            } else {
                ProfileName::parse(name)?
            };
            return Ok(exit_code(app.run_profile(&name, &args)?));
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn confirm_removal(name: &ProfileName) -> Result<bool, io::Error> {
    if !io::stdin().is_terminal() {
        return Err(io::Error::other(
            "refusing to remove a profile without confirmation; pass --yes",
        ));
    }
    eprint!("Permanently remove profile `{}`? [y/N] ", name.as_str());
    io::stderr().flush()?;
    let mut response = String::new();
    io::stdin().read_line(&mut response)?;
    Ok(matches!(
        response.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_ineligible(recommendation: &codex_handoff::BestRecommendation) {
    eprintln!("no eligible profile:");
    for evaluation in &recommendation.evaluations {
        eprintln!("  {}: {}", evaluation.profile.as_str(), evaluation.reason);
    }
}

fn exit_code(status: ExitStatus) -> ExitCode {
    ExitCode::from(
        status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(1),
    )
}
