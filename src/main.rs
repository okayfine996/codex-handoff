use clap::{Parser, Subcommand};
use codex_handoff::{
    App, AppPaths, AppServerProbe, AppServerUsageReader, ProfileName, SystemLoginRunner,
    SystemProcessGuard,
};
use std::{path::PathBuf, process::ExitCode};

mod presentation;

#[derive(Parser)]
#[command(
    name = "ch",
    version,
    about = "Safely hand off local Codex auth.json profiles"
)]
struct Cli {
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
    Init,
    Add {
        #[arg(short, long)]
        force: bool,
    },
    Relogin {
        name: String,
        #[arg(short, long)]
        force: bool,
    },
    Switch {
        name: String,
        #[arg(short, long, conflicts_with = "force")]
        close_clients: bool,
        #[arg(short, long, conflicts_with = "close_clients")]
        force: bool,
    },
    Sync {
        #[arg(short, long)]
        force: bool,
    },
    List,
    Status,
    Doctor,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ch: {error}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    .with_usage_reader(Box::new(AppServerUsageReader::from_path(cli.codex_bin)));
    match cli.command {
        Command::Init => {
            app.init()?;
            println!("saved the current Codex authentication as the active profile");
        }
        Command::Add { force } => {
            let name = app.add(force)?;
            println!(
                "saved profile `{}`; the previous profile remains active",
                name.as_str()
            );
        }
        Command::Relogin { name, force } => {
            app.relogin(ProfileName::parse(name)?, force)?;
            println!("updated the profile; the previous profile remains active");
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
        Command::List => {
            let output = presentation::render_list(&app.list()?);
            if output.is_empty() {
                println!("No profiles saved. Run `ch init` first.");
            } else {
                println!("{output}");
            }
        }
        Command::Status => {
            let status = app.status()?;
            println!(
                "{}",
                presentation::render_status(
                    &status.active,
                    &app.current_live_usage()?,
                    &app.paths().live_auth_path(),
                    app.paths().handoff_home(),
                )
            );
        }
        Command::Doctor => {
            for item in app.doctor() {
                println!("{item}");
            }
        }
    }
    Ok(())
}
