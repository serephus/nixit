//! Repository configuration parsed from a flake's `githubRepositories` output.
//!
//! Every setting is optional: a missing field means "leave it alone". This is
//! what makes `nixit` a reconciler rather than a full-state overwriter.
//!
//! The schema is grouped so that related options sit together and features use
//! the idiomatic Nix `<feature>.enable` shape.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Repository configuration from a flake's `githubRepositories` output.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Repositories keyed by their `githubRepositories` attribute key.
    pub repos: BTreeMap<String, RepoConfig>,
}

impl Config {
    /// Parse and validate a `githubRepositories` JSON object.
    pub fn from_json(json: &str) -> Result<Self> {
        let config: Config = serde_json::from_str(json).context("parsing `githubRepositories`")?;
        config.validate()?;
        Ok(config)
    }

    /// Evaluate a flake's `githubRepositories` output into a configuration.
    ///
    /// `reference` is a flake reference with an optional `#<repo>` fragment,
    /// e.g. `.#myrepo`, `github:me/repos#myrepo`, or `.#` for every repository.
    /// Each repository may set `name` (defaulting to the attribute key) and its
    /// own `owner`.
    pub fn from_flake(reference: &str) -> Result<Self> {
        let (flake_ref, repo) = split_flake_ref(reference)?;
        // This expression is kept deliberately small. Passing the ref and repo
        // through the environment avoids any escaping concerns, and
        // `builtins.getAttr` handles repository names that are not valid Nix
        // identifiers (for example `my-repo`).
        let expr = r#"
let
  flake = builtins.getFlake (builtins.getEnv "NIXIT_REF");
  repos = flake.githubRepositories or (throw "flake has no `githubRepositories` output");
  repo = builtins.getEnv "NIXIT_REPO";
in {
  repos = if repo == "" then repos else { ${repo} = builtins.getAttr repo repos; };
}
"#;
        let output = std::process::Command::new("nix")
            .args([
                "eval",
                "--impure",
                "--json",
                "--extra-experimental-features",
                "nix-command flakes",
                "--expr",
                expr,
            ])
            .env("NIXIT_REF", &flake_ref)
            .env("NIXIT_REPO", &repo)
            .output()
            .context("running `nix eval`; is Nix on PATH?")?;
        if !output.status.success() {
            bail!(
                "evaluating flake `{reference}` failed:\n{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let text =
            String::from_utf8(output.stdout).context("`nix eval` returned non-UTF-8 output")?;
        Self::from_json(&text)
            .with_context(|| format!("reading `githubRepositories` from flake `{reference}`"))
    }

    fn validate(&self) -> Result<()> {
        if self.repos.is_empty() {
            bail!("configuration declares no repositories");
        }
        for (key, repo) in &self.repos {
            validate_repo(key, repo)?;
        }
        Ok(())
    }
}

/// Semantic validation that serde cannot express on its own.
fn validate_repo(key: &str, repo: &RepoConfig) -> Result<()> {
    let name = repo.name.as_deref().unwrap_or(key);
    if name.trim().is_empty() {
        bail!("repository name cannot be empty");
    }
    if name.contains('/') {
        bail!("repository `{name}`: use the short name, not `owner/name`");
    }
    if let Some(owner) = &repo.owner
        && owner.trim().is_empty()
    {
        bail!("repository `{name}`: `owner` cannot be empty");
    }
    if repo.visibility == Some(Visibility::Internal) {
        bail!(
            "repository `{name}`: `internal` visibility is only available to \
             enterprise organizations, not personal accounts"
        );
    }
    if let Some(topics) = &repo.topics {
        for topic in topics {
            let valid = !topic.is_empty()
                && topic.len() <= 50
                && topic
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
            if !valid {
                bail!(
                    "repository `{name}`: invalid topic `{topic}` (topics are 1-50 \
                     characters of [A-Za-z0-9._-])"
                );
            }
        }
    }
    if let Some(pull) = &repo.pull {
        // GitHub requires at least one merge method. Only reject an explicit
        // "all disabled"; undeclared methods are left as they are.
        let methods = merge_methods(pull);
        if methods.iter().all(|method| *method == Some(false)) {
            bail!(
                "repository `{name}`: at least one of `pull.merge.enable`, \
                 `pull.squash.enable`, or `pull.rebase.enable` must be true"
            );
        }
        if pull.auto_merge == Some(true) && methods.iter().all(|method| *method != Some(true)) {
            bail!(
                "repository `{name}`: `pull.auto_merge = true` requires `pull.merge.enable` or \
                 `pull.squash.enable` to be true"
            );
        }
    }
    if let Some(branches) = &repo.branch_protection {
        for branch in branches.keys() {
            if branch.trim().is_empty() {
                bail!("repository `{name}`: branch names cannot be empty");
            }
        }
    }
    if let Some(actions) = &repo.actions
        && actions.policy == Some(AllowedActions::Selected)
        && actions.selected.is_none()
    {
        bail!(
            "repository `{name}`: `actions.policy = \"selected\"` also requires \
             `actions.selected`"
        );
    }
    Ok(())
}

fn merge_methods(pull: &PullRequestConfig) -> [Option<bool>; 3] {
    [
        pull.merge.as_ref().and_then(|method| method.enable),
        pull.squash.as_ref().and_then(|method| method.enable),
        pull.rebase.as_ref().and_then(|method| method.enable),
    ]
}

/// Settings for a single repository.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// Repository name on GitHub. Defaults to the configuration attribute key.
    pub name: Option<String>,

    /// Login of the account that owns the repository. Falls back to the
    /// authenticated user.
    pub owner: Option<String>,

    // --- Basic metadata -------------------------------------------------
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub topics: Option<Vec<String>>,
    pub visibility: Option<Visibility>,

    // --- Feature toggles ------------------------------------------------
    pub features: Option<FeaturesConfig>,

    // --- Repository state ------------------------------------------------
    pub is_template: Option<bool>,
    pub is_archived: Option<bool>,

    // --- Pull requests and merging --------------------------------------
    pub pull: Option<PullRequestConfig>,

    // --- Actions --------------------------------------------------------
    pub actions: Option<ActionsConfig>,

    // --- Branch protection, keyed by branch name ------------------------
    pub branch_protection: Option<BTreeMap<String, BranchProtectionConfig>>,
}

/// Feature switches, each following the Nix `<feature>.enable` convention.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeaturesConfig {
    pub wiki: Option<Toggle>,
    pub issues: Option<Toggle>,
    pub projects: Option<Toggle>,
    pub discussions: Option<Toggle>,
}

/// A simple feature switch, mirroring the Nix `<feature>.enable` convention.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    pub enable: Option<bool>,
}

/// Visibility of a repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Public,
    Private,
    Internal,
}

/// Pull request and merge behaviour.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PullRequestConfig {
    /// Allow merge commits.
    pub merge: Option<Toggle>,
    /// Allow squash merges.
    pub squash: Option<Toggle>,
    /// Allow rebase merges.
    pub rebase: Option<Toggle>,
    /// Allow auto-merge.
    pub auto_merge: Option<bool>,
    /// Delete the head branch after merging.
    pub delete_branch_on_merge: Option<bool>,
    /// Allow updating pull request branches.
    pub update_branch: Option<bool>,
}

/// GitHub Actions permissions for a repository.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionsConfig {
    /// Whether Actions are enabled at all.
    pub enable: Option<bool>,
    /// Which actions may run. `selected` requires `selected`.
    pub policy: Option<AllowedActions>,
    /// Allow-list configuration, only meaningful when `policy = "selected"`.
    pub selected: Option<SelectedActionsConfig>,
    /// Default permissions granted to `GITHUB_TOKEN` in workflows.
    pub default_token_permissions: Option<WorkflowPermission>,
    /// Whether Actions may create or approve pull request reviews.
    pub allow_pr_approval: Option<bool>,
}

/// Which actions are allowed to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowedActions {
    All,
    LocalOnly,
    Selected,
}

/// Allow-list when `policy = "selected"`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedActionsConfig {
    /// Allow actions authored by GitHub.
    pub github_owned: Option<bool>,
    /// Allow actions from verified creators.
    pub verified: Option<bool>,
    /// Allow actions matching these patterns, e.g. `actions/*`.
    pub patterns: Option<Vec<String>>,
}

/// Default permissions granted to `GITHUB_TOKEN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowPermission {
    Read,
    Write,
}

/// Classic branch protection for one branch.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchProtectionConfig {
    /// Require a linear commit history (no merge commits).
    pub required_linear_history: Option<bool>,
    /// Apply the rules to administrators too.
    pub enforce_admins: Option<bool>,
    /// Allow force pushes to the branch.
    pub allow_force_pushes: Option<bool>,
    /// Allow the branch to be deleted.
    pub allow_deletions: Option<bool>,
    /// Block branch creation matching the pattern.
    pub block_creations: Option<bool>,
    /// Require all conversations to be resolved before merging.
    pub required_conversation_resolution: Option<bool>,
    /// Lock the branch as read-only.
    pub lock_branch: Option<bool>,
    /// Allow fork syncing.
    pub allow_fork_syncing: Option<bool>,
    /// Require signed commits. Applied through a separate API endpoint.
    pub required_signatures: Option<bool>,
    /// Require status checks to pass before merging.
    pub required_status_checks: Option<RequiredStatusChecks>,
    /// Require reviews from pull requests before merging.
    pub required_pull_request_reviews: Option<RequiredPullRequestReviews>,
}

/// Required status checks.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredStatusChecks {
    /// Require the branch to be up to date before merging.
    pub strict: Option<bool>,
    /// Status check names/contexts that must pass.
    pub contexts: Option<Vec<String>>,
}

/// Required pull request reviews.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequiredPullRequestReviews {
    /// Dismiss approvals when new commits are pushed.
    pub dismiss_stale_reviews: Option<bool>,
    /// Require review from a code owner.
    pub require_code_owner_reviews: Option<bool>,
    /// Number of approving reviews required.
    pub required_approving_review_count: Option<u32>,
    /// Require approval of the most recent reviewable push.
    pub require_last_push_approval: Option<bool>,
}

/// Split `[flake-ref]#[repo]`, resolving local paths to absolute ones so
/// `builtins.getFlake` accepts them.
fn split_flake_ref(reference: &str) -> Result<(String, String)> {
    let (flake, repo) = match reference.split_once('#') {
        Some((flake, repo)) => (flake, repo),
        None => (reference, ""),
    };
    let flake = if flake.is_empty() { "." } else { flake };
    let flake = if flake.starts_with('.') || flake.starts_with('/') {
        std::fs::canonicalize(flake)
            .with_context(|| format!("resolving flake path `{flake}`"))?
            .to_string_lossy()
            .into_owned()
    } else {
        flake.to_string()
    };
    Ok((flake, repo.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_remote_and_local_flake_refs() {
        let (flake, repo) = split_flake_ref("github:me/repos#myrepo").unwrap();
        assert_eq!(flake, "github:me/repos");
        assert_eq!(repo, "myrepo");

        let (flake, repo) = split_flake_ref("github:me/repos").unwrap();
        assert_eq!(flake, "github:me/repos");
        assert_eq!(repo, "");

        let (flake, repo) = split_flake_ref(".#myrepo").unwrap();
        assert!(
            flake.starts_with('/'),
            "expected an absolute path, got {flake}"
        );
        assert_eq!(repo, "myrepo");
    }
}
