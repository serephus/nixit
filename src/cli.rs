//! Command line interface and top-level orchestration.

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use colored::Colorize;
use palc::Parser;

use crate::config::Config;
use crate::github::{DEFAULT_BASE_URL, HttpApi};
use crate::plan::{self, RepoPlan};
use crate::{apply, display};

/// `nixit` command line interface.
#[derive(Debug, Parser)]
#[command(
    name = "nixit",
    long_about = "Declaratively manage GitHub repository settings from a Nix flake."
)]
struct Cli {
    /// GitHub personal access token (falls back to $GITHUB_TOKEN).
    #[arg(long)]
    token: Option<String>,

    /// Flake reference to read `githubRepositories` from, e.g. `.#myrepo`.
    #[arg(long)]
    flake: Option<String>,

    /// Print the planned changes without touching GitHub.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Suppress per-repository progress output.
    #[arg(short, long)]
    quiet: bool,
}

/// Parse the arguments and run `nixit`, returning the process exit code.
pub fn run() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{} {error:#}", "error:".red().bold());
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<()> {
    let config = resolve_config(&cli)?;
    let token = resolve_token(&cli)?;
    // `NIXIT_GITHUB_API_URL` overrides the API endpoint (development/tests).
    let base =
        std::env::var("NIXIT_GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let api = HttpApi::new(token, base)?;

    let plans = plan::build_plan(&api, &config)?;
    if cli.dry_run {
        display::print_plan(&plans);
        return Ok(());
    }

    // The authenticated user is only needed when a repository may be created.
    let login = if plans.iter().any(|plan| !plan.exists) {
        Some(
            api.login()
                .context("determining the authenticated user before creating repositories")?,
        )
    } else {
        None
    };

    let mut failures = Vec::new();
    for plan in &plans {
        let target = format!("{}/{}", plan.owner, plan.repo);

        // Create a missing repository first, then re-read it so the settings we
        // apply reflect the freshly created repository.
        let created: RepoPlan;
        let plan = if plan.exists {
            plan
        } else {
            match create_repository(&api, &config, plan, login.as_deref()) {
                Ok(fresh) => {
                    if !cli.quiet {
                        println!("{} {target}", "created:".green());
                    }
                    created = fresh;
                    &created
                }
                Err(failure) => {
                    failures.push((target, failure));
                    continue;
                }
            }
        };

        if !cli.quiet {
            for warning in &plan.warnings {
                eprintln!("{} {target}: {warning}", "warning:".yellow().bold());
            }
        }

        if !plan.has_changes() {
            if !cli.quiet {
                println!("up to date: {target}");
            }
            continue;
        }

        let repo_failures = apply::sync_repo(&api, plan);
        if repo_failures.is_empty() {
            if !cli.quiet {
                println!("{} {target}", "applied:".green());
            }
        } else {
            failures.extend(
                repo_failures
                    .into_iter()
                    .map(|failure| (target.clone(), failure)),
            );
        }
    }

    report_failures(&failures)
}

/// Create a repository that the configuration declares but GitHub does not have.
///
/// Only the authenticated user's own repositories can be created. The result is
/// re-read so the caller applies settings to the actual repository state.
fn create_repository(
    api: &HttpApi,
    config: &Config,
    plan: &RepoPlan,
    login: Option<&str>,
) -> std::result::Result<RepoPlan, apply::Failure> {
    let login = login.expect("the login is fetched when a repository is missing");
    if plan.owner != login {
        return Err(apply::Failure {
            operation: "create repository".to_string(),
            error: anyhow!(
                "`nixit` can only create repositories for the authenticated user `{login}`; \
                 `{}` is owned by someone else",
                plan.owner
            ),
        });
    }
    let spec = plan
        .create_spec
        .as_ref()
        .expect("a missing repository has a create spec");
    api.create_repo(spec).map_err(|error| apply::Failure {
        operation: "create repository".to_string(),
        error,
    })?;
    plan::plan_repo(api, &plan.owner, &plan.key, &config.repos[&plan.key]).map_err(|error| {
        apply::Failure {
            operation: "read created repository".to_string(),
            error,
        }
    })
}

fn report_failures(failures: &[(String, apply::Failure)]) -> Result<()> {
    if failures.is_empty() {
        return Ok(());
    }
    for (target, failure) in failures {
        eprintln!(
            "{} {target}: {}: {:#}",
            "error:".red().bold(),
            failure.operation,
            failure.error
        );
    }
    bail!("{} operation(s) failed", failures.len())
}

fn resolve_config(cli: &Cli) -> Result<Config> {
    match &cli.flake {
        Some(reference) => Config::from_flake(reference),
        None => bail!("no flake given; pass `--flake <REF>`"),
    }
}

fn resolve_token(cli: &Cli) -> Result<String> {
    if let Some(token) = &cli.token {
        return Ok(token.clone());
    }
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        return Ok(token);
    }
    bail!("no GitHub token found; set $GITHUB_TOKEN or pass --token")
}
