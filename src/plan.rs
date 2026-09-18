//! Pure reconciliation logic: compare declared config against the live state
//! and produce a plan that both `--dry-run` and the real run consume.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config::{
    ActionsConfig, AllowedActions, BranchProtectionConfig, Config, RepoConfig, Toggle, Visibility,
    WorkflowPermission,
};
use crate::github::{
    ActionsPermissions, BranchProtection, CreateRepo, HttpApi, Repo, SelectedActions,
    WorkflowPermissions, merge_branch_protection,
};

/// A single user-visible difference.
#[derive(Debug, Clone)]
pub struct Change {
    pub scope: String,
    pub field: String,
    /// `None` means the value is not currently set / the repo does not exist.
    pub from: Option<String>,
    pub to: String,
}

impl Change {
    fn new(scope: &str, field: &str, from: Option<String>, to: impl Into<String>) -> Self {
        Self {
            scope: scope.to_string(),
            field: field.to_string(),
            from,
            to: to.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ActionsPlan {
    pub permissions: Option<ActionsPermissions>,
    pub workflow: Option<WorkflowPermissions>,
    pub selected: Option<SelectedActions>,
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct BranchPlan {
    pub branch: String,
    pub protection: Option<BranchProtection>,
    pub required_signatures: Option<bool>,
    pub changes: Vec<Change>,
}

/// Everything to reconcile for one repository.
#[derive(Debug, Clone)]
pub struct RepoPlan {
    /// Configuration attribute key (the resolved repository name may differ).
    pub key: String,
    pub owner: String,
    pub repo: String,
    pub exists: bool,
    pub create_spec: Option<CreateRepo>,
    pub settings: Option<Map<String, Value>>,
    pub topics: Option<Vec<String>>,
    pub actions: Option<ActionsPlan>,
    pub branches: Vec<BranchPlan>,
    pub changes: Vec<Change>,
    /// Non-fatal notes, e.g. branch protection skipped on an empty repository.
    pub warnings: Vec<String>,
}

impl RepoPlan {
    pub fn has_changes(&self) -> bool {
        !self.changes.is_empty()
    }
}

/// Build a plan for every repository in the configuration.
///
/// A repository's owner comes from its own `owner` field, falling back to the
/// authenticated user.
pub fn build_plan(api: &HttpApi, config: &Config) -> Result<Vec<RepoPlan>> {
    // The authenticated user is only needed for repositories without an owner.
    let login = if config.repos.values().any(|cfg| cfg.owner.is_none()) {
        Some(api.login()?)
    } else {
        None
    };

    let mut plans = Vec::new();
    for (key, cfg) in &config.repos {
        let owner = cfg
            .owner
            .clone()
            .or_else(|| login.clone())
            .with_context(|| format!("no owner for repository `{key}`"))?;
        plans.push(plan_repo(api, &owner, key, cfg)?);
    }
    Ok(plans)
}

/// Build a plan for a single repository.
///
/// `key` is the configuration attribute key; the GitHub repository name is
/// `cfg.name` when set, otherwise `key`.
pub fn plan_repo(api: &HttpApi, owner: &str, key: &str, cfg: &RepoConfig) -> Result<RepoPlan> {
    let name = repo_name(key, cfg);
    let current = api.get_repo(owner, name)?;
    let exists = current.is_some();
    let repo = current.unwrap_or_default();

    // Actions state is only needed when the configuration declares any actions
    // settings. GitHub answers `409 Conflict` for `selected-actions` when the
    // repository policy is not `selected`, so only ask for it when relevant.
    let (perms, wf, sel) = if exists && cfg.actions.is_some() {
        let perms = api
            .get_actions_permissions(owner, name)?
            .unwrap_or_default();
        let wf = api
            .get_workflow_permissions(owner, name)?
            .unwrap_or_default();
        let wants_selected = perms.allowed_actions == AllowedActions::Selected
            || cfg.actions.as_ref().and_then(|a| a.policy) == Some(AllowedActions::Selected);
        let sel = if wants_selected {
            api.get_selected_actions(owner, name)?.unwrap_or_default()
        } else {
            SelectedActions::default()
        };
        (perms, wf, sel)
    } else {
        (
            ActionsPermissions::default(),
            WorkflowPermissions::default(),
            SelectedActions::default(),
        )
    };

    let mut branches = Vec::new();
    let mut warnings = Vec::new();
    if let Some(branch_cfgs) = &cfg.branch_protection {
        for (branch, bcfg) in branch_cfgs {
            // A repository that does not exist yet (or has no commits) has no
            // branch, so branch protection cannot be applied.
            if !exists || !api.branch_exists(owner, name, branch)? {
                warnings.push(format!(
                    "branch `{branch}` does not exist yet; branch protection was skipped (run `nixit` after the first commit)"
                ));
                continue;
            }
            let protection = api.get_branch_protection(owner, name, branch)?;
            let signatures = api.get_required_signatures(owner, name, branch)?;
            branches.push(plan_branch(branch, bcfg, protection, signatures));
        }
    }

    let (settings, mut changes) = settings_patch(cfg, &repo);
    let topics = match topics_change(cfg, &repo.topics) {
        Some(change) => {
            changes.push(change);
            cfg.topics.clone()
        }
        None => None,
    };
    let actions = cfg
        .actions
        .as_ref()
        .and_then(|a| plan_actions(a, perms, wf, sel));
    if let Some(a) = &actions {
        changes.extend(a.changes.iter().cloned());
    }
    for b in &branches {
        changes.extend(b.changes.iter().cloned());
    }

    let create_spec = (!exists).then(|| build_create_spec(name, cfg));
    if !exists {
        changes.insert(0, Change::new("repository", "create", None, "create"));
    }

    Ok(RepoPlan {
        key: key.to_string(),
        owner: owner.to_string(),
        repo: name.to_string(),
        exists,
        create_spec,
        settings: (!settings.is_empty()).then_some(settings),
        topics,
        actions,
        branches,
        changes,
        warnings,
    })
}

/// Compute the `PATCH /repos` fields that differ from the live state.
fn settings_patch(cfg: &RepoConfig, repo: &Repo) -> (Map<String, Value>, Vec<Change>) {
    let mut patch = Map::new();
    let mut changes = Vec::new();

    if let Some(vis) = cfg.visibility {
        let desired = vis == Visibility::Private;
        if desired != repo.private {
            patch.insert("private".to_string(), Value::Bool(desired));
            changes.push(Change::new(
                "",
                "visibility",
                Some(if repo.private { "private" } else { "public" }.to_string()),
                visibility_str(vis),
            ));
        }
    }

    push_string(
        &mut patch,
        &mut changes,
        "",
        "description",
        &cfg.description,
        &repo.description,
    );
    push_string(
        &mut patch,
        &mut changes,
        "",
        "homepage",
        &cfg.homepage,
        &repo.homepage,
    );

    if let Some(features) = &cfg.features {
        push_toggle(
            &mut patch,
            &mut changes,
            "features.wiki",
            features.wiki.as_ref(),
            "has_wiki",
            repo.has_wiki.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.issues",
            features.issues.as_ref(),
            "has_issues",
            repo.has_issues.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.projects",
            features.projects.as_ref(),
            "has_projects",
            repo.has_projects.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.discussions",
            features.discussions.as_ref(),
            "has_discussions",
            repo.has_discussions.unwrap_or(false),
        );
    }

    push_bool(
        &mut patch,
        &mut changes,
        "",
        "is_template",
        "is_template",
        cfg.is_template,
        repo.is_template.unwrap_or(false),
    );
    push_bool(
        &mut patch,
        &mut changes,
        "",
        "is_archived",
        "archived",
        cfg.is_archived,
        repo.archived.unwrap_or(false),
    );

    if let Some(pull) = &cfg.pull {
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.merge",
            pull.merge.as_ref(),
            "allow_merge_commit",
            repo.allow_merge_commit.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.squash",
            pull.squash.as_ref(),
            "allow_squash_merge",
            repo.allow_squash_merge.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.rebase",
            pull.rebase.as_ref(),
            "allow_rebase_merge",
            repo.allow_rebase_merge.unwrap_or(true),
        );
        push_bool(
            &mut patch,
            &mut changes,
            "pull",
            "auto_merge",
            "allow_auto_merge",
            pull.auto_merge,
            repo.allow_auto_merge.unwrap_or(false),
        );
        push_bool(
            &mut patch,
            &mut changes,
            "pull",
            "delete_branch_on_merge",
            "delete_branch_on_merge",
            pull.delete_branch_on_merge,
            repo.delete_branch_on_merge.unwrap_or(false),
        );
        push_bool(
            &mut patch,
            &mut changes,
            "pull",
            "update_branch",
            "allow_update_branch",
            pull.update_branch,
            repo.allow_update_branch.unwrap_or(false),
        );
    }

    (patch, changes)
}

fn push_toggle(
    patch: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    scope: &str,
    toggle: Option<&Toggle>,
    api_field: &str,
    current: bool,
) {
    let declared = toggle.and_then(|toggle| toggle.enable);
    push_bool(
        patch, changes, scope, "enable", api_field, declared, current,
    );
}

fn topics_change(cfg: &RepoConfig, current: &[String]) -> Option<Change> {
    let declared = cfg.topics.as_ref()?;
    let a: BTreeSet<&String> = declared.iter().collect();
    let b: BTreeSet<&String> = current.iter().collect();
    if a == b {
        return None;
    }
    let from = (!current.is_empty()).then(|| current.join(", "));
    Some(Change::new("", "topics", from, declared.join(", ")))
}

fn plan_actions(
    cfg: &ActionsConfig,
    perms: ActionsPermissions,
    wf: WorkflowPermissions,
    sel: SelectedActions,
) -> Option<ActionsPlan> {
    let scope = "actions";
    let mut changes = Vec::new();

    let desired_perms = ActionsPermissions {
        enabled: cfg.enable.unwrap_or(perms.enabled),
        allowed_actions: cfg.policy.unwrap_or(perms.allowed_actions),
    };
    if desired_perms.enabled != perms.enabled {
        changes.push(Change::new(
            scope,
            "enable",
            Some(perms.enabled.to_string()),
            desired_perms.enabled.to_string(),
        ));
    }
    if desired_perms.allowed_actions != perms.allowed_actions {
        changes.push(Change::new(
            scope,
            "policy",
            Some(allowed_actions_str(perms.allowed_actions)),
            allowed_actions_str(desired_perms.allowed_actions),
        ));
    }

    let desired_wf = WorkflowPermissions {
        default_workflow_permissions: cfg
            .default_token_permissions
            .unwrap_or(wf.default_workflow_permissions),
        can_approve_pull_request_reviews: cfg
            .allow_pr_approval
            .unwrap_or(wf.can_approve_pull_request_reviews),
    };
    if desired_wf.default_workflow_permissions != wf.default_workflow_permissions {
        changes.push(Change::new(
            scope,
            "default_token_permissions",
            Some(workflow_permission_str(wf.default_workflow_permissions)),
            workflow_permission_str(desired_wf.default_workflow_permissions),
        ));
    }
    if desired_wf.can_approve_pull_request_reviews != wf.can_approve_pull_request_reviews {
        changes.push(Change::new(
            scope,
            "allow_pr_approval",
            Some(wf.can_approve_pull_request_reviews.to_string()),
            desired_wf.can_approve_pull_request_reviews.to_string(),
        ));
    }

    // Allow-lists only matter once the policy is (or becomes) `selected`.
    let selected = cfg.selected.as_ref().and_then(|scfg| {
        if desired_perms.allowed_actions != AllowedActions::Selected {
            return None;
        }
        let desired = SelectedActions {
            github_owned_allowed: scfg.github_owned.unwrap_or(sel.github_owned_allowed),
            verified_allowed: scfg.verified.unwrap_or(sel.verified_allowed),
            patterns_allowed: scfg
                .patterns
                .clone()
                .unwrap_or_else(|| sel.patterns_allowed.clone()),
        };
        if desired == sel {
            return None;
        }
        changes.push(Change::new(
            scope,
            "selected",
            Some(format!(
                "github_owned={}, verified={}, patterns=[{}]",
                sel.github_owned_allowed,
                sel.verified_allowed,
                sel.patterns_allowed.join(", ")
            )),
            format!(
                "github_owned={}, verified={}, patterns=[{}]",
                desired.github_owned_allowed,
                desired.verified_allowed,
                desired.patterns_allowed.join(", ")
            ),
        ));
        Some(desired)
    });

    if changes.is_empty() {
        return None;
    }
    Some(ActionsPlan {
        permissions: (desired_perms != perms).then_some(desired_perms),
        workflow: (desired_wf != wf).then_some(desired_wf),
        selected,
        changes,
    })
}

macro_rules! cmp_bool_fields {
    ($changes:expr, $scope:expr, $base:expr, $desired:expr, $($field:ident),+ $(,)?) => {
        $(
            cmp_bool(
                &mut $changes,
                $scope,
                stringify!($field),
                $base.$field,
                $desired.$field,
            );
        )+
    };
}

fn plan_branch(
    branch: &str,
    cfg: &BranchProtectionConfig,
    current: Option<BranchProtection>,
    signatures: bool,
) -> BranchPlan {
    let base = current.unwrap_or_default();
    let desired = merge_branch_protection(&base, cfg);
    let scope = format!("branch_protection.{branch}");
    let mut changes = Vec::new();

    cmp_bool_fields!(
        changes,
        &scope,
        base,
        desired,
        enforce_admins,
        required_linear_history,
        allow_force_pushes,
        allow_deletions,
        block_creations,
        required_conversation_resolution,
        lock_branch,
        allow_fork_syncing,
    );

    if cfg.required_status_checks.is_some()
        && desired.required_status_checks != base.required_status_checks
    {
        changes.push(Change::new(
            &scope,
            "required_status_checks",
            Some(json_opt(&base.required_status_checks)),
            json_opt(&desired.required_status_checks),
        ));
    }
    if cfg.required_pull_request_reviews.is_some()
        && desired.required_pull_request_reviews != base.required_pull_request_reviews
    {
        changes.push(Change::new(
            &scope,
            "required_pull_request_reviews",
            Some(json_opt(&base.required_pull_request_reviews)),
            json_opt(&desired.required_pull_request_reviews),
        ));
    }

    let required_signatures = cfg.required_signatures.filter(|d| *d != signatures);
    if let Some(d) = required_signatures {
        changes.push(Change::new(
            &scope,
            "required_signatures",
            Some(signatures.to_string()),
            d.to_string(),
        ));
    }

    // `required_signatures` is applied through its own endpoint, so only PUT the
    // bulk protection object when something else changed.
    let protection = changes
        .iter()
        .any(|c| c.field != "required_signatures")
        .then_some(desired);

    BranchPlan {
        branch: branch.to_string(),
        protection,
        required_signatures,
        changes,
    }
}

/// The GitHub repository name for a configuration entry: the explicit `name`,
/// or the configuration attribute key when it is absent.
fn repo_name<'a>(key: &'a str, cfg: &'a RepoConfig) -> &'a str {
    cfg.name.as_deref().unwrap_or(key)
}

fn build_create_spec(name: &str, cfg: &RepoConfig) -> CreateRepo {
    CreateRepo {
        name: name.to_string(),
        description: cfg.description.clone(),
        homepage: cfg.homepage.clone(),
        private: cfg.visibility == Some(Visibility::Private),
    }
}

// --- small helpers ----------------------------------------------------------

fn push_bool(
    patch: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    scope: &str,
    config_field: &str,
    api_field: &str,
    declared: Option<bool>,
    current: bool,
) {
    if let Some(d) = declared
        && d != current
    {
        patch.insert(api_field.to_string(), Value::Bool(d));
        changes.push(Change::new(
            scope,
            config_field,
            Some(current.to_string()),
            d.to_string(),
        ));
    }
}

fn push_string(
    patch: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    scope: &str,
    field: &str,
    declared: &Option<String>,
    current: &Option<String>,
) {
    if let Some(d) = declared
        && current.as_deref() != Some(d.as_str())
    {
        patch.insert(field.to_string(), Value::String(d.clone()));
        changes.push(Change::new(scope, field, current.clone(), d.clone()));
    }
}

fn cmp_bool(changes: &mut Vec<Change>, scope: &str, field: &str, from: bool, to: bool) {
    if from != to {
        changes.push(Change::new(
            scope,
            field,
            Some(from.to_string()),
            to.to_string(),
        ));
    }
}

fn json_opt<T: Serialize>(value: &Option<T>) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn visibility_str(v: Visibility) -> String {
    match v {
        Visibility::Public => "public",
        Visibility::Private => "private",
        Visibility::Internal => "internal",
    }
    .to_string()
}

fn allowed_actions_str(v: AllowedActions) -> String {
    match v {
        AllowedActions::All => "all",
        AllowedActions::LocalOnly => "local_only",
        AllowedActions::Selected => "selected",
    }
    .to_string()
}

fn workflow_permission_str(v: WorkflowPermission) -> String {
    match v {
        WorkflowPermission::Read => "read",
        WorkflowPermission::Write => "write",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ActionsConfig, FeaturesConfig, PullRequestConfig, SelectedActionsConfig, Toggle,
    };

    fn toggle(enable: bool) -> Option<Toggle> {
        Some(Toggle {
            enable: Some(enable),
        })
    }

    #[test]
    fn repository_name_defaults_to_the_key_and_can_be_overridden() {
        assert_eq!(repo_name("label", &RepoConfig::default()), "label");
        let cfg = RepoConfig {
            name: Some("actual".into()),
            ..Default::default()
        };
        assert_eq!(repo_name("label", &cfg), "actual");
    }

    #[test]
    fn settings_emit_only_declared_changes() {
        let cfg = RepoConfig {
            features: Some(FeaturesConfig {
                wiki: toggle(false),
                ..Default::default()
            }),
            pull: Some(PullRequestConfig {
                merge: toggle(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &Repo::default());
        assert_eq!(patch.len(), 2);
        assert_eq!(patch.get("has_wiki"), Some(&Value::Bool(false)));
        assert_eq!(patch.get("allow_merge_commit"), Some(&Value::Bool(false)));
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn settings_skip_values_that_already_match_defaults() {
        let cfg = RepoConfig {
            features: Some(FeaturesConfig {
                wiki: toggle(true),
                issues: toggle(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &Repo::default());
        assert!(patch.is_empty(), "unexpected patch: {patch:?}");
        assert!(changes.is_empty());
    }

    #[test]
    fn settings_map_visibility_to_private_flag() {
        let cfg = RepoConfig {
            visibility: Some(Visibility::Private),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &Repo::default());
        assert_eq!(patch.get("private"), Some(&Value::Bool(true)));
        assert_eq!(changes[0].field, "visibility");
    }

    #[test]
    fn topics_are_order_insensitive() {
        let cfg = RepoConfig {
            topics: Some(vec!["b".into(), "a".into()]),
            ..Default::default()
        };
        let current = vec!["a".to_string(), "b".to_string()];
        assert!(topics_change(&cfg, &current).is_none());
    }

    #[test]
    fn topics_report_additions() {
        let cfg = RepoConfig {
            topics: Some(vec!["a".into(), "b".into()]),
            ..Default::default()
        };
        let change = topics_change(&cfg, &["a".to_string()]).unwrap();
        assert_eq!(change.from.as_deref(), Some("a"));
        assert_eq!(change.to, "a, b");
    }

    #[test]
    fn branch_linear_history_triggers_full_put() {
        let cfg = BranchProtectionConfig {
            required_linear_history: Some(true),
            ..Default::default()
        };
        let plan = plan_branch("main", &cfg, None, false);
        let protection = plan.protection.expect("protection should be PUT");
        assert!(protection.required_linear_history);
        assert!(plan.required_signatures.is_none());
    }

    #[test]
    fn branch_signatures_only_does_not_put_protection() {
        let cfg = BranchProtectionConfig {
            required_signatures: Some(true),
            ..Default::default()
        };
        let plan = plan_branch("main", &cfg, None, false);
        assert!(plan.protection.is_none());
        assert_eq!(plan.required_signatures, Some(true));
    }

    #[test]
    fn merge_preserves_undeclared_fields() {
        let current = BranchProtection {
            required_linear_history: true,
            enforce_admins: true,
            allow_force_pushes: true,
            ..Default::default()
        };
        let cfg = BranchProtectionConfig {
            required_linear_history: Some(false),
            ..Default::default()
        };
        let merged = merge_branch_protection(&current, &cfg);
        assert!(!merged.required_linear_history);
        assert!(merged.enforce_admins, "enforce_admins must be preserved");
        assert!(
            merged.allow_force_pushes,
            "allow_force_pushes must be preserved"
        );
    }

    #[test]
    fn actions_selected_policy_produces_permissions_and_allowlist() {
        let cfg = ActionsConfig {
            policy: Some(AllowedActions::Selected),
            selected: Some(SelectedActionsConfig {
                patterns: Some(vec!["actions/*".into()]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let plan = plan_actions(
            &cfg,
            ActionsPermissions::default(),
            WorkflowPermissions::default(),
            SelectedActions::default(),
        )
        .expect("expected an actions plan");
        assert!(plan.permissions.is_some());
        assert!(plan.selected.is_some());
    }

    #[test]
    fn create_spec_is_minimal_and_empty() {
        let spec = build_create_spec("new", &RepoConfig::default());
        assert_eq!(spec.name, "new");
        assert!(!spec.private);
        assert_eq!(spec.description, None);
    }

    #[test]
    fn create_spec_honours_metadata() {
        let cfg = RepoConfig {
            description: Some("desc".into()),
            visibility: Some(Visibility::Private),
            ..Default::default()
        };
        let spec = build_create_spec("new", &cfg);
        assert_eq!(spec.description.as_deref(), Some("desc"));
        assert!(spec.private);
    }
}
