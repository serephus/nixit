//! HTTP-layer tests against a mocked GitHub API.
//!
//! `reqwest::blocking` owns an internal runtime that must be created and
//! dropped outside of a tokio context, so every test builds and uses its
//! client inside a plain OS thread.

use nixit::github::{BranchProtection, CreateRepo, HttpApi};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn blocking<T, F>(f: F) -> T
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.await.expect("blocking thread panicked")
}

#[tokio::test]
async fn get_repo_returns_none_on_404() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/missing"))
        .and(header("authorization", "Bearer secret"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let repo = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.get_repo("me", "missing")
    })
    .await
    .unwrap();
    assert!(repo.is_none());
}

#[tokio::test]
async fn get_repo_parses_settings() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "nixit",
            "full_name": "me/nixit",
            "private": false,
            "has_wiki": true,
            "allow_merge_commit": true,
            "merge_commit_title": "PR_TITLE",
            "squash_merge_commit_message": "COMMIT_MESSAGES",
            "web_commit_signoff_required": true,
            "allow_forking": true,
            "topics": ["nix", "github"]
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let repo = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.get_repo("me", "nixit")
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(repo.name, "nixit");
    assert_eq!(repo.topics, vec!["nix", "github"]);
    assert_eq!(repo.merge_commit_title.as_deref(), Some("PR_TITLE"));
    assert_eq!(
        repo.squash_merge_commit_message.as_deref(),
        Some("COMMIT_MESSAGES")
    );
    assert_eq!(repo.web_commit_signoff_required, Some(true));
    assert_eq!(repo.allow_forking, Some(true));
}

#[tokio::test]
async fn create_repo_posts_expected_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/user/repos"))
        .and(body_json(json!({
            "name": "nixit",
            "private": false
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "name": "nixit",
            "full_name": "me/nixit"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let repo = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let spec = CreateRepo {
            name: "nixit".into(),
            description: None,
            homepage: None,
            private: false,
        };
        api.create_repo(&spec)
    })
    .await
    .unwrap();
    assert_eq!(repo.name, "nixit");
}

#[tokio::test]
async fn update_repo_patches_only_given_fields() {
    let server = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/repos/me/nixit"))
        .and(body_json(json!({ "has_wiki": false })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "name": "nixit",
            "full_name": "me/nixit"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let mut patch = serde_json::Map::new();
        patch.insert("has_wiki".to_string(), Value::Bool(false));
        api.update_repo("me", "nixit", &patch)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn branch_protection_put_sends_full_object() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/repos/me/nixit/branches/main/protection"))
        .and(body_json(json!({
            "required_status_checks": null,
            "enforce_admins": true,
            "required_pull_request_reviews": null,
            "restrictions": null,
            "required_linear_history": true,
            "allow_force_pushes": false,
            "allow_deletions": false,
            "block_creations": false,
            "required_conversation_resolution": false,
            "lock_branch": false,
            "allow_fork_syncing": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let uri = server.uri();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let protection = BranchProtection {
            required_linear_history: true,
            enforce_admins: true,
            ..Default::default()
        };
        api.set_branch_protection("me", "nixit", "main", &protection)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn required_signatures_toggle_uses_post_and_delete() {
    let server = MockServer::start().await;
    let sig_path = "/repos/me/nixit/branches/main/protection/required_signatures";
    Mock::given(method("POST"))
        .and(path(sig_path))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "enabled": true })))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path(sig_path))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let uri = server.uri();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.set_required_signatures("me", "nixit", "main", true)
    })
    .await
    .unwrap();

    let uri = server.uri();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.set_required_signatures("me", "nixit", "main", false)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn api_errors_are_reported_with_the_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "message": "Resource not accessible by personal access token"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let error = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.get_repo("me", "nixit")
    })
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Resource not accessible by personal access token"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn branch_exists_maps_404_to_false() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "name": "main" })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit/branches/ghost"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({ "message": "Branch not found" })),
        )
        .mount(&server)
        .await;

    let uri = server.uri();
    let (main, ghost) = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        (
            api.branch_exists("me", "nixit", "main").unwrap(),
            api.branch_exists("me", "nixit", "ghost").unwrap(),
        )
    })
    .await;
    assert!(main);
    assert!(!ghost);
}

#[tokio::test]
async fn errors_include_the_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "message": "Resource not accessible by personal access token"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let error = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.get_repo("me", "nixit")
    })
    .await
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("GET /repos/me/nixit"), "{message}");
    assert!(message.contains("403"), "{message}");
}

#[tokio::test]
async fn sync_repo_continues_after_a_failure() {
    let server = MockServer::start().await;
    // The settings PATCH fails...
    Mock::given(method("PATCH"))
        .and(path("/repos/me/nixit"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "Validation Failed"
        })))
        .mount(&server)
        .await;
    // ...but the topics update must still be attempted.
    Mock::given(method("PUT"))
        .and(path("/repos/me/nixit/topics"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "names": ["rust"] })))
        .expect(1)
        .mount(&server)
        .await;

    let uri = server.uri();
    let failures = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let mut settings = serde_json::Map::new();
        settings.insert("has_wiki".to_string(), Value::Bool(false));
        let plan = nixit::plan::RepoPlan {
            key: "nixit".to_string(),
            owner: "me".to_string(),
            repo: "nixit".to_string(),
            exists: true,
            create_spec: None,
            settings: Some(settings),
            topics: Some(vec!["rust".to_string()]),
            actions: None,
            branches: Vec::new(),
            rulesets: Vec::new(),
            changes: Vec::new(),
            warnings: Vec::new(),
        };
        nixit::apply::sync_repo(&api, &plan)
    })
    .await;

    assert_eq!(failures.len(), 1, "expected one recorded failure");
    assert_eq!(failures[0].operation, "update repository settings");
    server.verify().await;
}

#[tokio::test]
async fn sync_repo_creates_and_updates_rulesets() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/me/nixit/rulesets"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/me/nixit/rulesets/2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 2 })))
        .expect(1)
        .mount(&server)
        .await;

    let uri = server.uri();
    let failures = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let body = json!({
            "name": "main",
            "target": "branch",
            "enforcement": "active",
            "rules": []
        });
        let plan = nixit::plan::RepoPlan {
            key: "nixit".to_string(),
            owner: "me".to_string(),
            repo: "nixit".to_string(),
            exists: true,
            create_spec: None,
            settings: None,
            topics: None,
            actions: None,
            branches: Vec::new(),
            rulesets: vec![
                nixit::plan::RulesetPlan {
                    name: "main".to_string(),
                    id: None,
                    body: body.clone(),
                    changes: Vec::new(),
                },
                nixit::plan::RulesetPlan {
                    name: "release".to_string(),
                    id: Some(2),
                    body,
                    changes: Vec::new(),
                },
            ],
            changes: Vec::new(),
            warnings: Vec::new(),
        };
        nixit::apply::sync_repo(&api, &plan)
    })
    .await;

    assert!(failures.is_empty(), "unexpected failures: {failures:?}");
    server.verify().await;
}

#[tokio::test]
async fn selected_actions_conflict_is_treated_as_unset() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit/actions/permissions/selected-actions"))
        .respond_with(ResponseTemplate::new(409).set_body_json(json!({
            "message": "Conflict",
            "documentation_url": "https://docs.github.com/rest/actions/permissions"
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let selected = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.get_selected_actions("me", "nixit")
    })
    .await
    .unwrap();
    assert!(selected.is_none());
}

#[tokio::test]
async fn list_and_get_rulesets_parse_the_response() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit/rulesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 7, "name": "main" }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/me/nixit/rulesets/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 7,
            "name": "main",
            "target": "branch",
            "enforcement": "active",
            "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
            "bypass_actors": [],
            "rules": [{ "type": "deletion" }]
        })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let (summaries, ruleset) = blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        let summaries = api.list_rulesets("me", "nixit").unwrap();
        let ruleset = api.get_ruleset("me", "nixit", 7).unwrap();
        (summaries, ruleset)
    })
    .await;
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, 7);
    assert_eq!(ruleset.name, "main");
    assert_eq!(ruleset.target.as_deref(), Some("branch"));
    assert_eq!(ruleset.rules.as_ref().unwrap().len(), 1);
}

#[tokio::test]
async fn create_and_update_rulesets_send_the_full_body() {
    let server = MockServer::start().await;
    let body = json!({
        "name": "main",
        "target": "branch",
        "enforcement": "active",
        "rules": [{ "type": "deletion" }]
    });
    Mock::given(method("POST"))
        .and(path("/repos/me/nixit/rulesets"))
        .and(body_json(body.clone()))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 9 })))
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/repos/me/nixit/rulesets/9"))
        .and(body_json(body.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 9 })))
        .mount(&server)
        .await;

    let uri = server.uri();
    let body2 = body.clone();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.create_ruleset("me", "nixit", &body)
    })
    .await
    .unwrap();

    let uri = server.uri();
    blocking(move || {
        let api = HttpApi::new("secret", uri).unwrap();
        api.update_ruleset("me", "nixit", 9, &body2)
    })
    .await
    .unwrap();
}
