//! Configuration parsing and validation tests.

use nixit::config::{
    AllowedActions, BypassActorType, Config, Enforcement, MergeCommitTitle, RulesetTarget,
    SquashCommitTitle, Visibility,
};

#[test]
fn parses_a_minimal_config() {
    let config = Config::from_json(
        r#"{"repos":{"nixit":{"features":{"wiki":{"enable":false}},"pull":{"squash":{"enable":true}}}}}"#,
    )
    .unwrap();
    let repo = &config.repos["nixit"];
    assert_eq!(
        repo.features
            .as_ref()
            .unwrap()
            .wiki
            .as_ref()
            .unwrap()
            .enable,
        Some(false)
    );
    assert_eq!(
        repo.pull.as_ref().unwrap().squash.as_ref().unwrap().enable,
        Some(true)
    );
    assert_eq!(repo.is_template, None);
    assert_eq!(repo.owner, None);
    assert_eq!(repo.name, None);
}

#[test]
fn parses_an_explicit_repository_name() {
    let config = Config::from_json(r#"{"repos":{"label":{"name":"nixit"}}}"#).unwrap();
    assert_eq!(config.repos["label"].name.as_deref(), Some("nixit"));
}

#[test]
fn rejects_unknown_fields_to_catch_typos() {
    let error = Config::from_json(r#"{"repos":{"nixit":{"has_wiki":false}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"), "{error}");
}

#[test]
fn rejects_owner_at_the_top_level() {
    // The owner lives on each repository, not at the top level.
    let error = Config::from_json(r#"{"owner":"serephus","repos":{"nixit":{}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"), "{error}");
}

#[test]
fn rejects_internal_visibility_for_personal_repos() {
    let error = Config::from_json(r#"{"repos":{"x":{"visibility":"internal"}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("internal"), "{error}");
}

#[test]
fn rejects_selected_policy_without_allowlist() {
    let error =
        Config::from_json(r#"{"repos":{"x":{"actions":{"policy":"selected"}}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("actions.selected"), "{error}");
}

#[test]
fn rejects_disabling_every_merge_method() {
    let error = Config::from_json(
        r#"{"repos":{"x":{"pull":{"merge":{"enable":false},"squash":{"enable":false},"rebase":{"enable":false}}}}}"#,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("at least one"), "{error}");
}

#[test]
fn rejects_auto_merge_without_a_merge_method() {
    let error = Config::from_json(
        r#"{"repos":{"x":{"pull":{"merge":{"enable":false},"squash":{"enable":false},"auto_merge":true}}}}"#,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("pull.auto_merge"), "{error}");
}

#[test]
fn rejects_invalid_topics() {
    let error = Config::from_json(r#"{"repos":{"x":{"topics":["has space"]}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("invalid topic"), "{error}");
}

#[test]
fn rejects_invalid_merge_commit_title() {
    let error = Config::from_json(r#"{"repos":{"x":{"pull":{"merge":{"commit_title":"nope"}}}}}"#)
        .unwrap_err();
    assert!(format!("{error:#}").contains("unknown variant"), "{error}");
}

#[test]
fn rejects_commit_options_on_rebase() {
    let error =
        Config::from_json(r#"{"repos":{"x":{"pull":{"rebase":{"commit_title":"pr_title"}}}}}"#)
            .unwrap_err();
    assert!(format!("{error:#}").contains("unknown field"), "{error}");
}

#[test]
fn rejects_unknown_ruleset_rule_types() {
    let error = Config::from_json(
        r#"{"repos":{"x":{"rulesets":{"main":{"rules":[{"type":"mystery"}]}}}}}"#,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("unknown rule type"),
        "{error}"
    );
}

#[test]
fn rejects_bypass_actors_without_an_id() {
    for actor_type in ["integration", "repository_role", "team", "user"] {
        let error = Config::from_json(&format!(
            r#"{{"repos":{{"x":{{"rulesets":{{"main":{{"bypass_actors":[{{"actor_type":"{actor_type}"}}]}}}}}}}}}}"#
        ))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("requires `actor_id`"),
            "{error}"
        );
    }
}

#[test]
fn rejects_rules_missing_parameters() {
    let error = Config::from_json(
        r#"{"repos":{"x":{"rulesets":{"main":{"rules":[{"type":"pull_request"}]}}}}}"#,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("requires `parameters`"),
        "{error}"
    );
}

#[test]
fn rejects_owner_slash_names() {
    let error = Config::from_json(r#"{"repos":{"me/repo":{}}}"#).unwrap_err();
    assert!(format!("{error:#}").contains("short name"), "{error}");
}

#[test]
fn parses_a_config_with_every_supported_setting() {
    let config = Config::from_json(include_str!("fixtures/full.json"))
        .expect("the full config must stay compatible");
    let repo = &config.repos["everything"];

    assert_eq!(repo.owner.as_deref(), Some("octocat"));
    assert_eq!(repo.name.as_deref(), Some("everything"));
    assert_eq!(repo.visibility, Some(Visibility::Private));
    assert_eq!(
        repo.features
            .as_ref()
            .unwrap()
            .wiki
            .as_ref()
            .unwrap()
            .enable,
        Some(false)
    );
    assert_eq!(
        repo.features
            .as_ref()
            .unwrap()
            .discussions
            .as_ref()
            .unwrap()
            .enable,
        Some(true)
    );
    assert_eq!(repo.is_template, Some(true));
    assert_eq!(repo.is_archived, Some(false));
    assert_eq!(repo.allow_forking, Some(false));

    let pull = repo.pull.as_ref().unwrap();
    assert_eq!(pull.squash.as_ref().unwrap().enable, Some(true));
    assert_eq!(
        pull.merge.as_ref().unwrap().commit_title,
        Some(MergeCommitTitle::PrTitle)
    );
    assert_eq!(
        pull.squash.as_ref().unwrap().commit_title,
        Some(SquashCommitTitle::CommitOrPrTitle)
    );
    assert_eq!(pull.delete_branch_on_merge, Some(true));
    assert_eq!(pull.web_commit_signoff_required, Some(true));

    let actions = repo.actions.as_ref().unwrap();
    assert_eq!(actions.policy, Some(AllowedActions::Selected));
    assert_eq!(
        actions
            .selected
            .as_ref()
            .unwrap()
            .patterns
            .as_ref()
            .unwrap(),
        &["actions/*".to_string()]
    );
    assert_eq!(actions.allow_pr_approval, Some(true));

    let ruleset = &repo.rulesets.as_ref().unwrap()["main"];
    assert_eq!(ruleset.target, Some(RulesetTarget::Branch));
    assert_eq!(ruleset.enforcement, Some(Enforcement::Active));
    assert_eq!(
        ruleset
            .conditions
            .as_ref()
            .unwrap()
            .ref_name
            .as_ref()
            .unwrap()
            .include,
        Some(vec!["~DEFAULT_BRANCH".to_string()])
    );
    assert_eq!(
        ruleset.bypass_actors.as_ref().unwrap()[0].actor_type,
        BypassActorType::RepositoryRole
    );
    let rules = ruleset.rules.as_ref().unwrap();
    assert_eq!(rules[0].rule_type, "deletion");
    assert_eq!(rules[2].rule_type, "pull_request");
    assert_eq!(
        rules[2].parameters.as_ref().unwrap()["required_approving_review_count"],
        1
    );
}
