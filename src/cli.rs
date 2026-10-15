//! Command line interface and top-level orchestration.

use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use colored::Colorize;
use palc::Parser;

use crate::config::Config;
use crate::github::{DEFAULT_BASE_URL, HttpApi};
use crate::plan::{self, RepoPlan};
use crate::{apply, display, export};

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

    /// Repository to export, as `owner/name`; repeat for several. `--export` only.
    #[arg(long = "repo", value_name = "OWNER/NAME")]
    repo: Vec<String>,

    /// Print the selected repositories' live settings as nixit configuration
    /// instead of applying changes.
    #[arg(long)]
    export: bool,

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
    let token = resolve_token(&cli)?;
    // `NIXIT_GITHUB_API_URL` overrides the API endpoint (development/tests).
    let base =
        std::env::var("NIXIT_GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    let api = HttpApi::new(token, base)?;

    if cli.export {
        let targets = export_targets(&api, &cli)?;
        print!("{}", export::export(&api, &targets)?);
        return Ok(());
    }

    let config = resolve_config(&cli)?;
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

/// Resolve which repositories `--export` should read.
///
/// With `--repo` the identities come straight from `owner/name` arguments;
/// otherwise the flake's `githubRepositories` keys are used for names and the
/// declared (or authenticated) owner.
fn export_targets(api: &HttpApi, cli: &Cli) -> Result<Vec<export::Target>> {
    if !cli.repo.is_empty() {
        if cli.flake.is_some() {
            bail!("`--repo` and `--flake` cannot be combined");
        }
        return cli
            .repo
            .iter()
            .map(|reference| {
                let (owner, name) = reference
                    .split_once('/')
                    .with_context(|| format!("`{reference}` is not `owner/name`"))?;
                if owner.is_empty() || name.is_empty() {
                    bail!("`{reference}` is not `owner/name`");
                }
                Ok(export::Target {
                    key: name.to_string(),
                    owner: owner.to_string(),
                    name: name.to_string(),
                })
            })
            .collect();
    }

    let config = resolve_config(cli)?;
    let login = if config.repos.values().any(|cfg| cfg.owner.is_none()) {
        Some(api.login()?)
    } else {
        None
    };
    config
        .repos
        .iter()
        .map(|(key, cfg)| {
            let owner = cfg
                .owner
                .clone()
                .or_else(|| login.clone())
                .with_context(|| format!("no owner for repository `{key}`"))?;
            Ok(export::Target {
                key: key.clone(),
                owner,
                name: cfg.name.clone().unwrap_or_else(|| key.clone()),
            })
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn api() -> HttpApi {
        HttpApi::new("token", "http://localhost").unwrap()
    }

    #[test]
    fn parses_repeatable_repo_arguments() {
        let cli =
            Cli::try_parse_from(["nixit", "--export", "--repo", "me/a", "--repo", "me/b"]).unwrap();
        assert!(cli.export);
        assert_eq!(cli.repo, vec!["me/a", "me/b"]);
    }

    #[test]
    fn export_rejects_combining_repo_and_flake() {
        let cli =
            Cli::try_parse_from(["nixit", "--export", "--flake", ".#x", "--repo", "me/a"]).unwrap();
        let error = export_targets(&api(), &cli).unwrap_err();
        assert!(error.to_string().contains("cannot be combined"), "{error}");
    }

    #[test]
    fn export_rejects_malformed_repo() {
        let cli = Cli::try_parse_from(["nixit", "--export", "--repo", "nope"]).unwrap();
        let error = export_targets(&api(), &cli).unwrap_err();
        assert!(error.to_string().contains("owner/name"), "{error}");
    }

    #[test]
    fn export_uses_repo_owner_and_name_as_the_key() {
        let cli = Cli::try_parse_from(["nixit", "--export", "--repo", "me/nixit"]).unwrap();
        let targets = export_targets(&api(), &cli).unwrap();
        assert_eq!(
            targets,
            vec![export::Target {
                key: "nixit".to_string(),
                owner: "me".to_string(),
                name: "nixit".to_string(),
            }]
        );
    }
}
