//! Pure reconciliation logic: compare declared config against the live state
//! and produce a plan that both `--dry-run` and the real run consume.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::config::{
    ActionsConfig, AllowedActions, BypassActor, BypassMode, Config, Enforcement, RepoConfig, Rule,
    RulesetConditions, RulesetConfig, RulesetTarget, Visibility, WorkflowPermission,
};
use crate::github::{
    ActionsPermissions, CreateRepo, HttpApi, Repo, Ruleset, SelectedActions, WorkflowPermissions,
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

/// A planned change to one repository ruleset.
#[derive(Debug, Clone)]
pub struct RulesetPlan {
    pub name: String,
    /// `None` means the ruleset does not exist yet and must be created.
    pub id: Option<u64>,
    /// Full body for `POST /rulesets` or `PUT /rulesets/{id}`.
    pub body: Value,
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
    pub rulesets: Vec<RulesetPlan>,
    pub changes: Vec<Change>,
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

    let rulesets = plan_rulesets(api, owner, name, exists, cfg)?;
    for ruleset in &rulesets {
        changes.extend(ruleset.changes.iter().cloned());
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
        rulesets,
        changes,
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
            features.wiki.as_ref().and_then(|t| t.enable),
            "has_wiki",
            repo.has_wiki.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.issues",
            features.issues.as_ref().and_then(|t| t.enable),
            "has_issues",
            repo.has_issues.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.projects",
            features.projects.as_ref().and_then(|t| t.enable),
            "has_projects",
            repo.has_projects.unwrap_or(true),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "features.discussions",
            features.discussions.as_ref().and_then(|t| t.enable),
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
    push_bool(
        &mut patch,
        &mut changes,
        "",
        "allow_forking",
        "allow_forking",
        cfg.allow_forking,
        repo.allow_forking.unwrap_or(true),
    );

    if let Some(pull) = &cfg.pull {
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.merge",
            pull.merge.as_ref().and_then(|m| m.enable),
            "allow_merge_commit",
            repo.allow_merge_commit.unwrap_or(true),
        );
        push_choice(
            &mut patch,
            &mut changes,
            "pull.merge",
            "commit_title",
            "merge_commit_title",
            pull.merge
                .as_ref()
                .and_then(|m| m.commit_title)
                .map(|t| t.api_str()),
            repo.merge_commit_title.as_deref(),
        );
        push_choice(
            &mut patch,
            &mut changes,
            "pull.merge",
            "commit_message",
            "merge_commit_message",
            pull.merge
                .as_ref()
                .and_then(|m| m.commit_message)
                .map(|m| m.api_str()),
            repo.merge_commit_message.as_deref(),
        );
        pair_title_with_message(
            &mut patch,
            "merge_commit_message",
            "merge_commit_title",
            repo.merge_commit_title.as_deref(),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.squash",
            pull.squash.as_ref().and_then(|m| m.enable),
            "allow_squash_merge",
            repo.allow_squash_merge.unwrap_or(true),
        );
        push_choice(
            &mut patch,
            &mut changes,
            "pull.squash",
            "commit_title",
            "squash_merge_commit_title",
            pull.squash
                .as_ref()
                .and_then(|m| m.commit_title)
                .map(|t| t.api_str()),
            repo.squash_merge_commit_title.as_deref(),
        );
        push_choice(
            &mut patch,
            &mut changes,
            "pull.squash",
            "commit_message",
            "squash_merge_commit_message",
            pull.squash
                .as_ref()
                .and_then(|m| m.commit_message)
                .map(|m| m.api_str()),
            repo.squash_merge_commit_message.as_deref(),
        );
        pair_title_with_message(
            &mut patch,
            "squash_merge_commit_message",
            "squash_merge_commit_title",
            repo.squash_merge_commit_title.as_deref(),
        );
        push_toggle(
            &mut patch,
            &mut changes,
            "pull.rebase",
            pull.rebase.as_ref().and_then(|t| t.enable),
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
        push_bool(
            &mut patch,
            &mut changes,
            "pull",
            "web_commit_signoff_required",
            "web_commit_signoff_required",
            pull.web_commit_signoff_required,
            repo.web_commit_signoff_required.unwrap_or(false),
        );
    }

    (patch, changes)
}

fn push_toggle(
    patch: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    scope: &str,
    declared: Option<bool>,
    api_field: &str,
    current: bool,
) {
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

fn plan_rulesets(
    api: &HttpApi,
    owner: &str,
    repo: &str,
    exists: bool,
    cfg: &RepoConfig,
) -> Result<Vec<RulesetPlan>> {
    let Some(ruleset_cfgs) = &cfg.rulesets else {
        return Ok(Vec::new());
    };

    let existing = if exists {
        api.list_rulesets(owner, repo)?
    } else {
        Vec::new()
    };

    let mut plans = Vec::new();
    for (key, ruleset) in ruleset_cfgs {
        let name = ruleset.name.as_deref().unwrap_or(key);
        let current = existing
            .iter()
            .find(|summary| summary.name == name)
            .map(|summary| api.get_ruleset(owner, repo, summary.id))
            .transpose()?;
        if let Some(plan) = plan_ruleset(key, name, ruleset, current.as_ref()) {
            plans.push(plan);
        }
    }
    Ok(plans)
}

/// Plan a single ruleset, or `None` when it already matches.
fn plan_ruleset(
    key: &str,
    name: &str,
    cfg: &RulesetConfig,
    current: Option<&Ruleset>,
) -> Option<RulesetPlan> {
    let scope = format!("rulesets.{key}");
    let body = build_ruleset_body(name, cfg, current);

    let Some(current) = current else {
        return Some(RulesetPlan {
            name: name.to_string(),
            id: None,
            body,
            changes: vec![Change::new(&scope, "create", None, "create")],
        });
    };

    let desired = canonicalize(&ruleset_desired(cfg));
    let live = canonicalize(&ruleset_current(current));
    if covers(&desired, &live) {
        return None;
    }

    let mut changes = Vec::new();
    diff(&scope, "", &desired, &live, &mut changes);
    Some(RulesetPlan {
        name: name.to_string(),
        id: Some(current.id),
        body,
        changes,
    })
}

/// Build the full ruleset body sent on create or update. Undeclared fields are
/// carried over from the live ruleset so an update never drops them.
fn build_ruleset_body(name: &str, cfg: &RulesetConfig, current: Option<&Ruleset>) -> Value {
    let mut body = Map::new();
    body.insert("name".to_string(), Value::String(name.to_string()));

    let target = cfg
        .target
        .map(RulesetTarget::api_str)
        .map(|value| Value::String(value.to_string()))
        .or_else(|| current.and_then(|c| c.target.clone()).map(Value::String))
        .unwrap_or_else(|| Value::String("branch".to_string()));
    body.insert("target".to_string(), target);

    let enforcement = cfg
        .enforcement
        .map(Enforcement::api_str)
        .map(|value| Value::String(value.to_string()))
        .or_else(|| current.map(|c| Value::String(c.enforcement.clone())))
        .unwrap_or_else(|| Value::String("active".to_string()));
    body.insert("enforcement".to_string(), enforcement);

    let current_conditions = current.and_then(|c| c.conditions.as_ref());
    match &cfg.conditions {
        Some(_) => {
            if let Some(conditions) = merge_conditions(cfg.conditions.as_ref(), current_conditions)
                && conditions
                    .as_object()
                    .is_some_and(|object| !object.is_empty())
            {
                body.insert("conditions".to_string(), conditions);
            }
        }
        None => {
            if let Some(conditions) = current_conditions {
                body.insert("conditions".to_string(), conditions.clone());
            }
        }
    }

    if let Some(actors) = &cfg.bypass_actors {
        body.insert(
            "bypass_actors".to_string(),
            Value::Array(actors.iter().map(bypass_actor_value).collect()),
        );
    } else if let Some(actors) = current.and_then(|c| c.bypass_actors.as_ref()) {
        body.insert("bypass_actors".to_string(), Value::Array(actors.clone()));
    }

    if let Some(rules) = &cfg.rules {
        body.insert(
            "rules".to_string(),
            Value::Array(rules.iter().map(rule_value).collect()),
        );
    } else if let Some(rules) = current.and_then(|c| c.rules.as_ref()) {
        body.insert("rules".to_string(), Value::Array(rules.clone()));
    }

    Value::Object(body)
}

/// The declared fields of a ruleset, used for change detection.
fn ruleset_desired(cfg: &RulesetConfig) -> Value {
    let mut map = Map::new();
    if let Some(target) = cfg.target {
        map.insert(
            "target".to_string(),
            Value::String(target.api_str().to_string()),
        );
    }
    if let Some(enforcement) = cfg.enforcement {
        map.insert(
            "enforcement".to_string(),
            Value::String(enforcement.api_str().to_string()),
        );
    }
    if let Some(conditions) = &cfg.conditions {
        let mut conditions_value = Map::new();
        if let Some(ref_name) = &conditions.ref_name {
            let mut patterns = Map::new();
            if let Some(include) = &ref_name.include {
                patterns.insert("include".to_string(), json_string_list(include));
            }
            if let Some(exclude) = &ref_name.exclude {
                patterns.insert("exclude".to_string(), json_string_list(exclude));
            }
            conditions_value.insert("ref_name".to_string(), Value::Object(patterns));
        }
        map.insert("conditions".to_string(), Value::Object(conditions_value));
    }
    if let Some(actors) = &cfg.bypass_actors {
        map.insert(
            "bypass_actors".to_string(),
            Value::Array(actors.iter().map(bypass_actor_value).collect()),
        );
    }
    if let Some(rules) = &cfg.rules {
        map.insert(
            "rules".to_string(),
            Value::Array(rules.iter().map(rule_value).collect()),
        );
    }
    Value::Object(map)
}

/// The managed fields of a live ruleset, used for change detection.
fn ruleset_current(current: &Ruleset) -> Value {
    let mut map = Map::new();
    if let Some(target) = &current.target {
        map.insert("target".to_string(), Value::String(target.clone()));
    }
    map.insert(
        "enforcement".to_string(),
        Value::String(current.enforcement.clone()),
    );
    if let Some(conditions) = &current.conditions {
        map.insert("conditions".to_string(), conditions.clone());
    }
    if let Some(actors) = &current.bypass_actors {
        map.insert("bypass_actors".to_string(), Value::Array(actors.clone()));
    }
    if let Some(rules) = &current.rules {
        map.insert("rules".to_string(), Value::Array(rules.clone()));
    }
    Value::Object(map)
}

/// Merge declared conditions over the live ones, so an update keeps whichever
/// of `include`/`exclude` was not declared.
///
/// GitHub requires both `include` and `exclude` under `ref_name`, so whenever a
/// `ref_name` condition is emitted the missing side defaults to an empty list
/// instead of being omitted. This matters on create, where there is no live
/// ruleset to fall back to and an omitted `exclude` fails with
/// `422 Validation Failed: "Missing required parameter \`exclude\`"`.
fn merge_conditions(cfg: Option<&RulesetConditions>, current: Option<&Value>) -> Option<Value> {
    let cfg = cfg?;
    let current_ref = current.and_then(|value| value.get("ref_name"));
    let declared_ref = cfg.ref_name.as_ref();
    let mut ref_name = Map::new();

    if declared_ref.is_some() || current_ref.is_some() {
        let include = declared_ref
            .and_then(|r| r.include.clone())
            .or_else(|| {
                current_ref
                    .and_then(|r| r.get("include"))
                    .and_then(string_list)
            })
            .unwrap_or_default();
        ref_name.insert("include".to_string(), json_string_list(&include));

        let exclude = declared_ref
            .and_then(|r| r.exclude.clone())
            .or_else(|| {
                current_ref
                    .and_then(|r| r.get("exclude"))
                    .and_then(string_list)
            })
            .unwrap_or_default();
        ref_name.insert("exclude".to_string(), json_string_list(&exclude));
    }

    let mut conditions = Map::new();
    if !ref_name.is_empty() {
        conditions.insert("ref_name".to_string(), Value::Object(ref_name));
    }
    Some(Value::Object(conditions))
}

fn bypass_actor_value(actor: &BypassActor) -> Value {
    let mut map = Map::new();
    map.insert(
        "actor_type".to_string(),
        Value::String(actor.actor_type.api_str().to_string()),
    );
    if actor.actor_type.needs_actor_id()
        && let Some(actor_id) = actor.actor_id
    {
        map.insert("actor_id".to_string(), Value::from(actor_id));
    }
    map.insert(
        "bypass_mode".to_string(),
        Value::String(
            actor
                .bypass_mode
                .unwrap_or(BypassMode::Always)
                .api_str()
                .to_string(),
        ),
    );
    Value::Object(map)
}

fn rule_value(rule: &Rule) -> Value {
    let mut map = Map::new();
    map.insert("type".to_string(), Value::String(rule.rule_type.clone()));
    if let Some(parameters) = &rule.parameters
        && !parameters.is_empty()
    {
        map.insert("parameters".to_string(), Value::Object(parameters.clone()));
    }
    Value::Object(map)
}

fn json_string_list(values: &[String]) -> Value {
    Value::Array(values.iter().map(|v| Value::String(v.clone())).collect())
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    })
}

/// Recursively sort arrays so that comparisons are order-insensitive.
fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => {
            let mut items: Vec<Value> = items.iter().map(canonicalize).collect();
            items.sort_by_key(sort_key);
            Value::Array(items)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn sort_key(value: &Value) -> String {
    if let Some(key) = value.get("type").and_then(Value::as_str) {
        return format!("0:{key}");
    }
    if let Some(key) = value.get("context").and_then(Value::as_str) {
        return format!("1:{key}");
    }
    if let Some(key) = value.get("actor_type").and_then(Value::as_str) {
        let id = value.get("actor_id").and_then(Value::as_u64).unwrap_or(0);
        return format!("2:{key}:{id}");
    }
    if let Some(key) = value.as_str() {
        return format!("3:{key}");
    }
    serde_json::to_string(value).unwrap_or_default()
}

/// Whether every declared value is present and equal in the live ruleset.
/// Extra fields returned by GitHub are ignored.
fn covers(desired: &Value, current: &Value) -> bool {
    match (desired, current) {
        (Value::Object(desired), Value::Object(current)) => desired
            .iter()
            .all(|(key, value)| current.get(key).is_some_and(|live| covers(value, live))),
        (Value::Array(desired), Value::Array(current)) => {
            desired.len() == current.len()
                && desired
                    .iter()
                    .zip(current)
                    .all(|(value, live)| covers(value, live))
        }
        _ => desired == current,
    }
}

fn diff(scope: &str, path: &str, desired: &Value, current: &Value, changes: &mut Vec<Change>) {
    if covers(desired, current) {
        return;
    }
    match (desired, current) {
        (Value::Object(desired), Value::Object(current)) => {
            for (key, value) in desired {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match current.get(key) {
                    Some(live) => diff(scope, &child, value, live, changes),
                    None => changes.push(Change::new(scope, &child, None, display_value(value))),
                }
            }
        }
        _ => changes.push(Change::new(
            scope,
            path,
            Some(display_value(current)),
            display_value(desired),
        )),
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
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

fn push_choice(
    patch: &mut Map<String, Value>,
    changes: &mut Vec<Change>,
    scope: &str,
    config_field: &str,
    api_field: &str,
    declared: Option<&str>,
    current: Option<&str>,
) {
    let Some(declared) = declared else {
        return;
    };
    if current == Some(declared) {
        return;
    }
    patch.insert(api_field.to_string(), Value::String(declared.to_string()));
    changes.push(Change::new(
        scope,
        config_field,
        current.map(str::to_lowercase),
        declared.to_lowercase(),
    ));
}

fn pair_title_with_message(
    patch: &mut Map<String, Value>,
    message_field: &str,
    title_field: &str,
    current_title: Option<&str>,
) {
    // GitHub requires the commit title whenever the commit message is set, so
    // carry the current title along when only the message was declared.
    if patch.contains_key(message_field)
        && !patch.contains_key(title_field)
        && let Some(title) = current_title
    {
        patch.insert(title_field.to_string(), Value::String(title.to_string()));
    }
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
        ActionsConfig, FeaturesConfig, MergeCommitMessage, MergeCommitTitle, MergeConfig,
        PullRequestConfig, RefNameCondition, SelectedActionsConfig, SquashCommitTitle,
        SquashConfig, Toggle,
    };
    use serde_json::json;

    fn toggle(enable: bool) -> Option<Toggle> {
        Some(Toggle {
            enable: Some(enable),
        })
    }

    fn merge(enable: bool) -> Option<MergeConfig> {
        Some(MergeConfig {
            enable: Some(enable),
            ..Default::default()
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
                merge: merge(false),
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
    fn merge_and_squash_commit_text_is_reconciled() {
        let cfg = RepoConfig {
            pull: Some(PullRequestConfig {
                merge: Some(MergeConfig {
                    commit_title: Some(MergeCommitTitle::PrTitle),
                    commit_message: Some(MergeCommitMessage::Blank),
                    ..Default::default()
                }),
                squash: Some(SquashConfig {
                    commit_title: Some(SquashCommitTitle::CommitOrPrTitle),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &Repo::default());
        assert_eq!(
            patch.get("merge_commit_title"),
            Some(&Value::String("PR_TITLE".into()))
        );
        assert_eq!(
            patch.get("merge_commit_message"),
            Some(&Value::String("BLANK".into()))
        );
        assert_eq!(
            patch.get("squash_merge_commit_title"),
            Some(&Value::String("COMMIT_OR_PR_TITLE".into()))
        );
        assert_eq!(changes.len(), 3);
    }

    #[test]
    fn declared_message_carries_the_current_title() {
        let cfg = RepoConfig {
            pull: Some(PullRequestConfig {
                merge: Some(MergeConfig {
                    commit_message: Some(MergeCommitMessage::Blank),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let repo = Repo {
            merge_commit_title: Some("PR_TITLE".into()),
            merge_commit_message: Some("PR_BODY".into()),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &repo);
        assert_eq!(
            patch.get("merge_commit_message"),
            Some(&Value::String("BLANK".into()))
        );
        assert_eq!(
            patch.get("merge_commit_title"),
            Some(&Value::String("PR_TITLE".into()))
        );
        assert_eq!(
            changes.len(),
            1,
            "only the message is a user-visible change"
        );
    }

    #[test]
    fn commit_text_matching_github_is_left_alone() {
        let cfg = RepoConfig {
            pull: Some(PullRequestConfig {
                merge: Some(MergeConfig {
                    commit_title: Some(MergeCommitTitle::MergeMessage),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let repo = Repo {
            merge_commit_title: Some("MERGE_MESSAGE".into()),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &repo);
        assert!(patch.is_empty(), "unexpected patch: {patch:?}");
        assert!(changes.is_empty());
    }

    #[test]
    fn repository_flags_are_reconciled() {
        let cfg = RepoConfig {
            allow_forking: Some(false),
            pull: Some(PullRequestConfig {
                web_commit_signoff_required: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (patch, changes) = settings_patch(&cfg, &Repo::default());
        assert_eq!(patch.get("allow_forking"), Some(&Value::Bool(false)));
        assert_eq!(
            patch.get("web_commit_signoff_required"),
            Some(&Value::Bool(true))
        );
        assert_eq!(changes.len(), 2);
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

    fn rule(rule_type: &str) -> Rule {
        Rule {
            rule_type: rule_type.to_string(),
            parameters: None,
        }
    }

    #[test]
    fn ruleset_is_created_when_absent() {
        let cfg = RulesetConfig {
            rules: Some(vec![rule("creation"), rule("deletion")]),
            ..Default::default()
        };
        let plan = plan_ruleset("main", "main", &cfg, None).expect("a new ruleset is planned");
        assert_eq!(plan.id, None);
        assert_eq!(plan.changes.len(), 1);
        assert_eq!(plan.changes[0].field, "create");
        assert_eq!(plan.body["target"], "branch");
        assert_eq!(plan.body["enforcement"], "active");
        assert_eq!(plan.body["rules"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ruleset_with_no_differences_is_skipped() {
        let cfg = RulesetConfig {
            target: Some(RulesetTarget::Branch),
            enforcement: Some(Enforcement::Active),
            conditions: Some(RulesetConditions {
                ref_name: Some(RefNameCondition {
                    include: Some(vec!["refs/heads/main".into()]),
                    exclude: None,
                }),
            }),
            rules: Some(vec![rule("deletion")]),
            ..Default::default()
        };
        let current = Ruleset {
            id: 7,
            name: "main".into(),
            target: Some("branch".into()),
            enforcement: "active".into(),
            bypass_actors: None,
            conditions: Some(json!({
                "ref_name": { "include": ["refs/heads/main"], "exclude": [] }
            })),
            rules: Some(vec![json!({ "type": "deletion" })]),
        };
        assert!(plan_ruleset("main", "main", &cfg, Some(&current)).is_none());
    }

    #[test]
    fn ruleset_ignores_extra_live_parameters() {
        let cfg = RulesetConfig {
            rules: Some(vec![Rule {
                rule_type: "commit_message_pattern".into(),
                parameters: Some(
                    json!({ "operator": "starts_with", "pattern": "feat" })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            }]),
            ..Default::default()
        };
        let current = Ruleset {
            id: 1,
            name: "main".into(),
            target: Some("branch".into()),
            enforcement: "active".into(),
            bypass_actors: None,
            conditions: None,
            rules: Some(vec![json!({
                "type": "commit_message_pattern",
                "parameters": {
                    "name": "Conventional commits",
                    "operator": "starts_with",
                    "pattern": "feat"
                }
            })]),
        };
        assert!(plan_ruleset("main", "main", &cfg, Some(&current)).is_none());
    }

    #[test]
    fn ruleset_update_replaces_rules_but_keeps_undeclared_fields() {
        let cfg = RulesetConfig {
            enforcement: Some(Enforcement::Disabled),
            rules: Some(vec![rule("creation")]),
            ..Default::default()
        };
        let current = Ruleset {
            id: 9,
            name: "main".into(),
            target: Some("tag".into()),
            enforcement: "active".into(),
            bypass_actors: Some(vec![json!({
                "actor_type": "OrganizationAdmin",
                "bypass_mode": "always"
            })]),
            conditions: Some(json!({
                "ref_name": { "include": ["refs/tags/*"], "exclude": [] }
            })),
            rules: Some(vec![json!({ "type": "deletion" })]),
        };
        let plan = plan_ruleset("main", "main", &cfg, Some(&current)).expect("expected update");
        assert_eq!(plan.id, Some(9));
        assert_eq!(plan.body["target"], "tag");
        assert_eq!(plan.body["enforcement"], "disabled");
        assert_eq!(plan.body["rules"], json!([{ "type": "creation" }]));
        assert_eq!(
            plan.body["bypass_actors"],
            json!([{ "actor_type": "OrganizationAdmin", "bypass_mode": "always" }])
        );
        assert!(plan.body["conditions"].is_object());
    }

    #[test]
    fn ruleset_conditions_merge_declared_side_only() {
        let cfg = RulesetConfig {
            conditions: Some(RulesetConditions {
                ref_name: Some(RefNameCondition {
                    include: Some(vec!["refs/heads/release/*".into()]),
                    exclude: None,
                }),
            }),
            ..Default::default()
        };
        let current = Ruleset {
            id: 1,
            name: "main".into(),
            target: Some("branch".into()),
            enforcement: "active".into(),
            bypass_actors: None,
            conditions: Some(json!({
                "ref_name": { "include": ["refs/heads/main"], "exclude": ["refs/heads/dev"] }
            })),
            rules: None,
        };
        let plan = plan_ruleset("main", "main", &cfg, Some(&current)).expect("expected update");
        assert_eq!(
            plan.body["conditions"],
            json!({
                "ref_name": {
                    "include": ["refs/heads/release/*"],
                    "exclude": ["refs/heads/dev"]
                }
            })
        );
    }

    #[test]
    fn ruleset_create_defaults_missing_ref_name_side_to_empty_list() {
        // Declaring only `include` must still send an `exclude` key.
        let include_only = RulesetConfig {
            conditions: Some(RulesetConditions {
                ref_name: Some(RefNameCondition {
                    include: Some(vec!["~DEFAULT_BRANCH".into()]),
                    exclude: None,
                }),
            }),
            rules: Some(vec![rule("deletion")]),
            ..Default::default()
        };
        let plan =
            plan_ruleset("main", "main", &include_only, None).expect("a new ruleset is planned");
        assert_eq!(
            plan.body["conditions"],
            json!({
                "ref_name": {
                    "include": ["~DEFAULT_BRANCH"],
                    "exclude": []
                }
            })
        );

        // ... and declaring only `exclude` must still send `include`.
        let exclude_only = RulesetConfig {
            conditions: Some(RulesetConditions {
                ref_name: Some(RefNameCondition {
                    include: None,
                    exclude: Some(vec!["refs/heads/dev".into()]),
                }),
            }),
            rules: Some(vec![rule("deletion")]),
            ..Default::default()
        };
        let plan =
            plan_ruleset("main", "main", &exclude_only, None).expect("a new ruleset is planned");
        assert_eq!(
            plan.body["conditions"],
            json!({
                "ref_name": {
                    "include": [],
                    "exclude": ["refs/heads/dev"]
                }
            })
        );
    }

    #[test]
    fn ruleset_create_without_ref_name_omits_conditions() {
        let cfg = RulesetConfig {
            rules: Some(vec![rule("deletion")]),
            ..Default::default()
        };
        let plan = plan_ruleset("main", "main", &cfg, None).expect("a new ruleset is planned");
        assert!(plan.body.get("conditions").is_none());
    }

    #[test]
    fn ruleset_arrays_compare_order_insensitively() {
        let cfg = RulesetConfig {
            rules: Some(vec![rule("deletion"), rule("creation")]),
            ..Default::default()
        };
        let current = Ruleset {
            id: 1,
            name: "main".into(),
            target: Some("branch".into()),
            enforcement: "active".into(),
            bypass_actors: None,
            conditions: None,
            rules: Some(vec![
                json!({ "type": "creation" }),
                json!({ "type": "deletion" }),
            ]),
        };
        assert!(plan_ruleset("main", "main", &cfg, Some(&current)).is_none());
    }
}
