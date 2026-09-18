//! Execution of a [`RepoPlan`] against the GitHub API.

use anyhow::Result;

use crate::github::HttpApi;
use crate::plan::RepoPlan;

/// A single failed operation while applying a plan.
#[derive(Debug)]
pub struct Failure {
    /// Human-readable description of what was being attempted.
    pub operation: String,
    pub error: anyhow::Error,
}

/// Apply every pending change in a per-repository plan.
///
/// Every operation is attempted even if an earlier one fails, so a single
/// rejected setting does not prevent the rest of the plan from being applied.
/// The caller reports the returned failures once the whole run is done.
pub fn sync_repo(api: &HttpApi, plan: &RepoPlan) -> Vec<Failure> {
    let mut failures = Vec::new();
    let owner = &plan.owner;
    let repo = &plan.repo;

    if let Some(patch) = &plan.settings
        && !patch.is_empty()
    {
        record(
            &mut failures,
            "update repository settings",
            api.update_repo(owner, repo, patch),
        );
    }

    if let Some(topics) = &plan.topics {
        record(
            &mut failures,
            "update topics",
            api.set_topics(owner, repo, topics),
        );
    }

    if let Some(actions) = &plan.actions {
        if let Some(perms) = &actions.permissions {
            record(
                &mut failures,
                "update actions permissions",
                api.set_actions_permissions(owner, repo, perms),
            );
        }
        if let Some(workflow) = &actions.workflow {
            record(
                &mut failures,
                "update workflow permissions",
                api.set_workflow_permissions(owner, repo, workflow),
            );
        }
        if let Some(selected) = &actions.selected {
            record(
                &mut failures,
                "update selected actions",
                api.set_selected_actions(owner, repo, selected),
            );
        }
    }

    for branch in &plan.branches {
        if let Some(protection) = &branch.protection {
            record(
                &mut failures,
                &format!("update branch protection for `{}`", branch.branch),
                api.set_branch_protection(owner, repo, &branch.branch, protection),
            );
        }
        if let Some(enabled) = branch.required_signatures {
            record(
                &mut failures,
                &format!("update required signatures for `{}`", branch.branch),
                api.set_required_signatures(owner, repo, &branch.branch, enabled),
            );
        }
    }

    failures
}

fn record(failures: &mut Vec<Failure>, operation: &str, result: Result<()>) {
    if let Err(error) = result {
        failures.push(Failure {
            operation: operation.to_string(),
            error,
        });
    }
}
