//! Read live repository settings from GitHub and render them as `nixit`
//! configuration.
//!
//! This is the inverse of the reconciler in [`crate::plan`]: instead of
//! building a patch from declared settings, it fetches every setting `nixit`
//! manages and prints a ready-to-paste Nix attribute set shaped like
//! `examples/repos.nix`.
//!
//! The output is a complete snapshot: absent-on-GitHub values fall back to the
//! same defaults the reconciler assumes, so applying the export is a no-op
//! until something on GitHub changes.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::config::{
    ActionsConfig, AllowedActions, BypassActor, BypassActorType, BypassMode, Enforcement,
    FeaturesConfig, MergeCommitMessage, MergeCommitTitle, MergeConfig, PullRequestConfig,
    RefNameCondition, RepoConfig, Rule, RulesetConditions, RulesetConfig, RulesetTarget,
    SelectedActionsConfig, SquashCommitMessage, SquashCommitTitle, SquashConfig, Toggle,
    Visibility,
};
use crate::github::{HttpApi, Repo, Ruleset};

/// A repository to export: its configuration key, owner and GitHub name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The key under `githubRepositories` (or `repos`) the snapshot uses.
    pub key: String,
    pub owner: String,
    pub name: String,
}

/// Fetch every target's live settings and render them as a Nix file.
pub fn export(api: &HttpApi, targets: &[Target]) -> Result<String> {
    let mut repos = BTreeMap::new();
    for target in targets {
        let repo = api
            .get_repo(&target.owner, &target.name)?
            .with_context(|| {
                format!(
                    "repository `{}/{}` does not exist or is not visible to the token",
                    target.owner, target.name
                )
            })?;
        repos.insert(target.key.clone(), export_repo(api, target, &repo)?);
    }
    Ok(render(&repos))
}

/// Read every setting nixit manages for one repository.
pub fn export_repo(api: &HttpApi, target: &Target, repo: &Repo) -> Result<RepoConfig> {
    let mut config = base_config(&target.owner, repo);
    config.actions = export_actions(api, target)?;
    config.rulesets = export_rulesets(api, target)?;
    Ok(config)
}

/// Map the `GET /repos/{owner}/{repo}` fields onto a configuration.
fn base_config(owner: &str, repo: &Repo) -> RepoConfig {
    RepoConfig {
        name: Some(repo.name.clone()),
        owner: Some(owner.to_string()),
        description: repo.description.clone(),
        homepage: repo.homepage.clone(),
        topics: Some(repo.topics.clone()),
        visibility: Some(if repo.private {
            Visibility::Private
        } else {
            Visibility::Public
        }),
        features: Some(FeaturesConfig {
            wiki: Some(toggle(repo.has_wiki.unwrap_or(true))),
            issues: Some(toggle(repo.has_issues.unwrap_or(true))),
            projects: Some(toggle(repo.has_projects.unwrap_or(true))),
            discussions: Some(toggle(repo.has_discussions.unwrap_or(false))),
        }),
        is_template: Some(repo.is_template.unwrap_or(false)),
        is_archived: Some(repo.archived.unwrap_or(false)),
        // GitHub only honours `allow_forking` on public repositories, so leave
        // it out for private ones to keep the output re-appliable.
        allow_forking: (!repo.private).then(|| repo.allow_forking.unwrap_or(true)),
        pull: Some(PullRequestConfig {
            merge: Some(MergeConfig {
                enable: Some(repo.allow_merge_commit.unwrap_or(true)),
                commit_title: repo
                    .merge_commit_title
                    .as_deref()
                    .and_then(MergeCommitTitle::from_api_str),
                commit_message: repo
                    .merge_commit_message
                    .as_deref()
                    .and_then(MergeCommitMessage::from_api_str),
            }),
            squash: Some(SquashConfig {
                enable: Some(repo.allow_squash_merge.unwrap_or(true)),
                commit_title: repo
                    .squash_merge_commit_title
                    .as_deref()
                    .and_then(SquashCommitTitle::from_api_str),
                commit_message: repo
                    .squash_merge_commit_message
                    .as_deref()
                    .and_then(SquashCommitMessage::from_api_str),
            }),
            rebase: Some(toggle(repo.allow_rebase_merge.unwrap_or(true))),
            auto_merge: Some(repo.allow_auto_merge.unwrap_or(false)),
            delete_branch_on_merge: Some(repo.delete_branch_on_merge.unwrap_or(false)),
            update_branch: Some(repo.allow_update_branch.unwrap_or(false)),
            web_commit_signoff_required: Some(repo.web_commit_signoff_required.unwrap_or(false)),
        }),
        actions: None,
        rulesets: None,
    }
}

/// Read the Actions permissions endpoints, if the token can see them.
fn export_actions(api: &HttpApi, target: &Target) -> Result<Option<ActionsConfig>> {
    let Some(perms) = api.get_actions_permissions(&target.owner, &target.name)? else {
        return Ok(None);
    };
    let workflow = api
        .get_workflow_permissions(&target.owner, &target.name)?
        .unwrap_or_default();
    // When the policy is `selected` an allow-list always exists; GitHub may
    // answer `409 Conflict` for it, which `get_selected_actions` maps to the
    // defaults. Emitting it keeps `actions.policy = "selected"` valid.
    let selected = if perms.allowed_actions == AllowedActions::Selected {
        let allow = api
            .get_selected_actions(&target.owner, &target.name)?
            .unwrap_or_default();
        Some(SelectedActionsConfig {
            github_owned: Some(allow.github_owned_allowed),
            verified: Some(allow.verified_allowed),
            patterns: Some(allow.patterns_allowed),
        })
    } else {
        None
    };
    Ok(Some(ActionsConfig {
        enable: Some(perms.enabled),
        policy: Some(perms.allowed_actions),
        selected,
        default_token_permissions: Some(workflow.default_workflow_permissions),
        allow_pr_approval: Some(workflow.can_approve_pull_request_reviews),
    }))
}

/// Read every ruleset, keyed by its GitHub name.
fn export_rulesets(
    api: &HttpApi,
    target: &Target,
) -> Result<Option<BTreeMap<String, RulesetConfig>>> {
    let summaries = api.list_rulesets_optional(&target.owner, &target.name)?;
    let Some(summaries) = summaries else {
        return Ok(None);
    };
    if summaries.is_empty() {
        return Ok(None);
    }
    let mut rulesets = BTreeMap::new();
    for summary in summaries {
        let live = api.get_ruleset(&target.owner, &target.name, summary.id)?;
        rulesets.insert(live.name.clone(), ruleset_config(&live));
    }
    Ok(Some(rulesets))
}

/// Map a live ruleset onto its configuration.
fn ruleset_config(live: &Ruleset) -> RulesetConfig {
    RulesetConfig {
        name: Some(live.name.clone()),
        target: live.target.as_deref().and_then(RulesetTarget::from_api_str),
        enforcement: Enforcement::from_api_str(&live.enforcement),
        conditions: live.conditions.as_ref().map(conditions_config),
        bypass_actors: live
            .bypass_actors
            .as_ref()
            .map(|actors| actors.iter().filter_map(bypass_actor).collect()),
        rules: live
            .rules
            .as_ref()
            .map(|rules| rules.iter().filter_map(rule).collect()),
    }
}

fn conditions_config(value: &Value) -> RulesetConditions {
    let ref_name = value.get("ref_name").map(|ref_name| RefNameCondition {
        include: string_list(ref_name.get("include")),
        exclude: string_list(ref_name.get("exclude")),
    });
    RulesetConditions { ref_name }
}

fn bypass_actor(value: &Value) -> Option<BypassActor> {
    let actor_type = BypassActorType::from_api_str(value.get("actor_type")?.as_str()?)?;
    Some(BypassActor {
        actor_id: value.get("actor_id").and_then(Value::as_u64),
        actor_type,
        bypass_mode: value
            .get("bypass_mode")
            .and_then(Value::as_str)
            .and_then(BypassMode::from_api_str),
    })
}

fn rule(value: &Value) -> Option<Rule> {
    Some(Rule {
        rule_type: value.get("type")?.as_str()?.to_string(),
        parameters: value.get("parameters").and_then(Value::as_object).cloned(),
    })
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect()
    })
}

fn toggle(enable: bool) -> Toggle {
    Toggle {
        enable: Some(enable),
    }
}

// ---------------------------------------------------------------------------
// Nix rendering
// ---------------------------------------------------------------------------

/// Render repositories as a Nix file shaped like `examples/repos.nix`, i.e. an
/// attribute set with a single `repos` attribute.
fn render(repos: &BTreeMap<String, RepoConfig>) -> String {
    let mut root = Map::new();
    root.insert(
        "repos".to_string(),
        serde_json::to_value(repos).expect("a repository config always serializes"),
    );
    let mut out = String::from("# Generated by `nixit --export`. Review before applying.\n");
    render_value(&Value::Object(root), 0, &mut out);
    out.push('\n');
    out
}

/// Render a JSON value as a Nix expression, skipping `null` fields.
fn render_value(value: &Value, indent: usize, out: &mut String) {
    match value {
        Value::Object(map) => {
            let fields: Vec<_> = map.iter().filter(|(_, value)| !value.is_null()).collect();
            if fields.is_empty() {
                out.push_str("{ }");
                return;
            }
            out.push_str("{\n");
            for (key, value) in fields {
                indent_line(out, indent + 1);
                out.push_str(&render_key(key));
                out.push_str(" = ");
                render_value(value, indent + 1, out);
                out.push_str(";\n");
            }
            indent_line(out, indent);
            out.push('}');
        }
        Value::Array(items) => {
            let items: Vec<_> = items.iter().filter(|item| !item.is_null()).collect();
            if items.is_empty() {
                out.push_str("[ ]");
                return;
            }
            out.push_str("[\n");
            for item in items {
                indent_line(out, indent + 1);
                render_value(item, indent + 1, out);
                out.push('\n');
            }
            indent_line(out, indent);
            out.push(']');
        }
        Value::String(text) => out.push_str(&render_string(text)),
        Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Value::Number(number) => {
            let _ = write!(out, "{number}");
        }
        Value::Null => out.push_str("null"),
    }
}

fn indent_line(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

/// Render an attribute name, quoting it when it is not a bare Nix identifier.
fn render_key(key: &str) -> String {
    if is_identifier(key) {
        key.to_string()
    } else {
        render_string(key)
    }
}

fn is_identifier(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '\''))
}

/// Render a double-quoted Nix string, escaping interpolation and specials.
fn render_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' if chars.peek() == Some(&'{') => {
                out.push_str("\\${");
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_repo() -> Repo {
        Repo {
            name: "nixit".into(),
            private: false,
            description: Some("desc".into()),
            homepage: Some("https://example.com".into()),
            topics: vec!["nix".into(), "github".into()],
            has_issues: Some(true),
            has_wiki: Some(false),
            has_projects: Some(false),
            has_discussions: Some(true),
            allow_merge_commit: Some(false),
            allow_squash_merge: Some(true),
            allow_rebase_merge: Some(false),
            allow_auto_merge: Some(true),
            delete_branch_on_merge: Some(true),
            allow_update_branch: Some(true),
            merge_commit_title: Some("PR_TITLE".into()),
            merge_commit_message: Some("BLANK".into()),
            squash_merge_commit_title: Some("COMMIT_OR_PR_TITLE".into()),
            squash_merge_commit_message: Some("COMMIT_MESSAGES".into()),
            web_commit_signoff_required: Some(true),
            allow_forking: Some(false),
            is_template: Some(true),
            archived: Some(false),
        }
    }

    #[test]
    fn base_config_inverts_repository_fields() {
        let config = base_config("octocat", &sample_repo());
        assert_eq!(config.name.as_deref(), Some("nixit"));
        assert_eq!(config.owner.as_deref(), Some("octocat"));
        assert_eq!(config.visibility, Some(Visibility::Public));
        assert_eq!(
            config.topics.as_deref(),
            Some(&["nix".into(), "github".into()][..])
        );
        assert_eq!(config.is_template, Some(true));
        assert_eq!(config.allow_forking, Some(false));

        let features = config.features.unwrap();
        assert_eq!(features.wiki.unwrap().enable, Some(false));
        assert_eq!(features.issues.unwrap().enable, Some(true));
        assert_eq!(features.discussions.unwrap().enable, Some(true));

        let pull = config.pull.unwrap();
        assert_eq!(pull.merge.as_ref().unwrap().enable, Some(false));
        assert_eq!(
            pull.merge.as_ref().unwrap().commit_title,
            Some(MergeCommitTitle::PrTitle)
        );
        assert_eq!(
            pull.merge.as_ref().unwrap().commit_message,
            Some(MergeCommitMessage::Blank)
        );
        assert_eq!(pull.rebase.unwrap().enable, Some(false));
        assert_eq!(pull.auto_merge, Some(true));
        assert_eq!(pull.web_commit_signoff_required, Some(true));
    }

    #[test]
    fn base_config_omits_allow_forking_for_private_repositories() {
        let repo = Repo {
            private: true,
            ..sample_repo()
        };
        assert_eq!(base_config("me", &repo).allow_forking, None);
    }

    #[test]
    fn ruleset_config_maps_live_values() {
        let live = Ruleset {
            id: 7,
            name: "main".into(),
            target: Some("branch".into()),
            enforcement: "active".into(),
            bypass_actors: Some(vec![json!({
                "actor_id": 5,
                "actor_type": "RepositoryRole",
                "bypass_mode": "always"
            })]),
            conditions: Some(json!({
                "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
            })),
            rules: Some(vec![json!({
                "type": "pull_request",
                "parameters": { "required_approving_review_count": 1 }
            })]),
        };
        let config = ruleset_config(&live);
        assert_eq!(config.name.as_deref(), Some("main"));
        assert_eq!(config.target, Some(RulesetTarget::Branch));
        assert_eq!(config.enforcement, Some(Enforcement::Active));

        let conditions = config.conditions.unwrap();
        assert_eq!(
            conditions.ref_name.unwrap().include,
            Some(vec!["~DEFAULT_BRANCH".to_string()])
        );

        let actor = &config.bypass_actors.unwrap()[0];
        assert_eq!(actor.actor_id, Some(5));
        assert_eq!(actor.actor_type, BypassActorType::RepositoryRole);
        assert_eq!(actor.bypass_mode, Some(BypassMode::Always));

        let rule = &config.rules.unwrap()[0];
        assert_eq!(rule.rule_type, "pull_request");
        assert_eq!(
            rule.parameters.as_ref().unwrap()["required_approving_review_count"],
            1
        );
    }

    #[test]
    fn render_produces_nix_with_escaped_strings_and_quoted_keys() {
        let mut repos = BTreeMap::new();
        repos.insert(
            "my repo".to_string(),
            RepoConfig {
                description: Some("say \"hi\" to ${name}".into()),
                topics: Some(vec!["a".into(), "b".into()]),
                ..Default::default()
            },
        );
        let out = render(&repos);
        assert!(out.contains("repos = {"), "{out}");
        assert!(out.contains("\"my repo\" = {"), "{out}");
        assert!(
            out.contains(r#"description = "say \"hi\" to \${name}";"#),
            "{out}"
        );
        assert!(out.contains("topics = ["), "{out}");
    }

    #[test]
    fn rendered_config_round_trips_through_validation() {
        let config = base_config("octocat", &sample_repo());
        let mut repos = BTreeMap::new();
        repos.insert("nixit".to_string(), config);
        // The serialized form must be accepted by the same parser the CLI uses.
        let json = json!({ "repos": serde_json::to_value(&repos).unwrap() }).to_string();
        crate::config::Config::from_json(&json).expect("exported config must be valid");
    }

    #[test]
    fn rendered_output_is_valid_nix() {
        if std::process::Command::new("nix-instantiate")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: `nix-instantiate` is not on PATH");
            return;
        }
        let mut repos = BTreeMap::new();
        repos.insert(
            "my-repo".to_string(),
            base_config("octocat", &sample_repo()),
        );
        repos.insert(
            "rules".to_string(),
            RepoConfig {
                rulesets: Some(BTreeMap::from([(
                    "main".to_string(),
                    ruleset_config(&Ruleset {
                        id: 1,
                        name: "main".into(),
                        target: Some("branch".into()),
                        enforcement: "active".into(),
                        bypass_actors: None,
                        conditions: Some(json!({
                            "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
                        })),
                        rules: Some(vec![json!({ "type": "deletion" })]),
                    }),
                )])),
                ..Default::default()
            },
        );
        let out = render(&repos);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.nix");
        std::fs::write(&path, &out).unwrap();
        let status = std::process::Command::new("nix-instantiate")
            .args(["--parse", path.to_str().unwrap()])
            .status()
            .expect("running nix-instantiate");
        assert!(status.success(), "nix could not parse:\n{out}");
    }

    #[test]
    fn rendered_output_is_accepted_by_the_nix_library() {
        if std::process::Command::new("nix-instantiate")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping: `nix-instantiate` is not on PATH");
            return;
        }
        let mut config = base_config("octocat", &sample_repo());
        config.actions = Some(ActionsConfig {
            enable: Some(true),
            policy: Some(AllowedActions::Selected),
            selected: Some(SelectedActionsConfig {
                github_owned: Some(true),
                verified: Some(false),
                patterns: Some(vec!["actions/*".into()]),
            }),
            default_token_permissions: Some(crate::config::WorkflowPermission::Read),
            allow_pr_approval: Some(false),
        });
        config.rulesets = Some(BTreeMap::from([(
            "main".to_string(),
            ruleset_config(&Ruleset {
                id: 1,
                name: "main".into(),
                target: Some("branch".into()),
                enforcement: "active".into(),
                bypass_actors: Some(vec![json!({
                    "actor_id": 5,
                    "actor_type": "RepositoryRole",
                    "bypass_mode": "always"
                })]),
                conditions: Some(json!({
                    "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] }
                })),
                rules: Some(vec![json!({ "type": "deletion" })]),
            }),
        )]));
        let mut repos = BTreeMap::new();
        repos.insert("my-repo".to_string(), config);
        let out = render(&repos);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repos.nix");
        std::fs::write(&path, &out).unwrap();
        let lib = concat!(env!("CARGO_MANIFEST_DIR"), "/nix/lib.nix");
        let expr = format!(
            "let lib = import {lib}; in lib.githubRepository (import {}).repos.my-repo",
            path.display()
        );
        let output = std::process::Command::new("nix-instantiate")
            .args(["--eval", "--strict", "--expr", &expr])
            .output()
            .expect("running nix-instantiate");
        assert!(
            output.status.success(),
            "nixit.lib.githubRepository rejected the export:\n{}\n\n{out}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
