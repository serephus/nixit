//! Thin wrapper around the GitHub REST API (github.com only).
//!
//! We talk to the raw REST endpoints rather than using a high-level client
//! because several of the endpoints we need (Actions permissions and branch
//! protection) are not consistently modelled by existing crates.

use anyhow::{Context, Result, bail};
use reqwest::Method;
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{AllowedActions, WorkflowPermission};

pub const DEFAULT_BASE_URL: &str = "https://api.github.com";

/// A repository as returned by `GET /repos/{owner}/{repo}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Repo {
    pub name: String,
    #[serde(default)]
    pub private: bool,
    pub description: Option<String>,
    pub homepage: Option<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    pub has_issues: Option<bool>,
    pub has_wiki: Option<bool>,
    pub has_projects: Option<bool>,
    pub has_discussions: Option<bool>,
    pub allow_merge_commit: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase_merge: Option<bool>,
    pub allow_auto_merge: Option<bool>,
    pub delete_branch_on_merge: Option<bool>,
    pub allow_update_branch: Option<bool>,
    pub merge_commit_title: Option<String>,
    pub merge_commit_message: Option<String>,
    pub squash_merge_commit_title: Option<String>,
    pub squash_merge_commit_message: Option<String>,
    pub web_commit_signoff_required: Option<bool>,
    pub allow_forking: Option<bool>,
    pub is_template: Option<bool>,
    pub archived: Option<bool>,
}

/// A repository ruleset summary from `GET /repos/{owner}/{repo}/rulesets`.
#[derive(Debug, Clone, Deserialize)]
pub struct RulesetSummary {
    pub id: u64,
    pub name: String,
}

/// A repository ruleset from `GET /repos/{owner}/{repo}/rulesets/{id}`.
///
/// Only the fields nixit manages are decoded; everything else is round-tripped
/// through [`serde_json::Value`] so the exact API representation is preserved.
#[derive(Debug, Clone, Deserialize)]
pub struct Ruleset {
    pub id: u64,
    pub name: String,
    pub target: Option<String>,
    pub enforcement: String,
    #[serde(default)]
    pub bypass_actors: Option<Vec<Value>>,
    #[serde(default)]
    pub conditions: Option<Value>,
    #[serde(default)]
    pub rules: Option<Vec<Value>>,
}

/// Body for `POST /user/repos`.
///
/// Repositories are always created empty, so there are no template or
/// auto-init fields.
#[derive(Debug, Clone, Serialize)]
pub struct CreateRepo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    pub private: bool,
}

/// Canonical actions permissions (`enabled` + allow-list policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionsPermissions {
    pub enabled: bool,
    pub allowed_actions: AllowedActions,
}

impl Default for ActionsPermissions {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_actions: AllowedActions::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPermissions {
    pub default_workflow_permissions: WorkflowPermission,
    #[serde(default)]
    pub can_approve_pull_request_reviews: bool,
}

impl Default for WorkflowPermissions {
    fn default() -> Self {
        Self {
            default_workflow_permissions: WorkflowPermission::Read,
            can_approve_pull_request_reviews: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedActions {
    pub github_owned_allowed: bool,
    pub verified_allowed: bool,
    pub patterns_allowed: Vec<String>,
}

impl Default for SelectedActions {
    fn default() -> Self {
        Self {
            github_owned_allowed: true,
            verified_allowed: false,
            patterns_allowed: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP client
// ---------------------------------------------------------------------------

/// Blocking GitHub REST client.
pub struct HttpApi {
    client: Client,
    token: String,
    base: String,
}

impl HttpApi {
    /// Build a client. `base` is normally [`DEFAULT_BASE_URL`].
    pub fn new(token: impl Into<String>, base: impl Into<String>) -> Result<Self> {
        let client = Client::builder()
            .user_agent(concat!("nixit/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            client,
            token: token.into(),
            base: base.into(),
        })
    }

    fn request(&self, method: Method, path: &str) -> ApiRequest {
        let builder = self
            .client
            .request(method.clone(), format!("{}{}", self.base, path))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        ApiRequest {
            builder,
            method,
            path: path.to_string(),
        }
    }

    /// Login of the authenticated user.
    pub fn login(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        Ok(self
            .request(Method::GET, "/user")
            .send_json::<User>()?
            .login)
    }

    pub fn get_repo(&self, owner: &str, repo: &str) -> Result<Option<Repo>> {
        self.request(Method::GET, &format!("/repos/{owner}/{repo}"))
            .send_optional()
    }

    pub fn create_repo(&self, spec: &CreateRepo) -> Result<Repo> {
        self.request(Method::POST, "/user/repos")
            .json(spec)
            .send_json()
    }

    pub fn update_repo(&self, owner: &str, repo: &str, patch: &Map<String, Value>) -> Result<()> {
        self.request(Method::PATCH, &format!("/repos/{owner}/{repo}"))
            .json(patch)
            .send_empty()
    }

    pub fn set_topics(&self, owner: &str, repo: &str, topics: &[String]) -> Result<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            names: &'a [String],
        }
        self.request(Method::PUT, &format!("/repos/{owner}/{repo}/topics"))
            .json(&Body { names: topics })
            .send_empty()
    }

    // --- Rulesets --------------------------------------------------------

    /// List every ruleset in the repository.
    pub fn list_rulesets(&self, owner: &str, repo: &str) -> Result<Vec<RulesetSummary>> {
        self.request(Method::GET, &format!("/repos/{owner}/{repo}/rulesets"))
            .send_json()
    }

    /// Read one ruleset, including its rules, conditions and bypass actors.
    pub fn get_ruleset(&self, owner: &str, repo: &str, id: u64) -> Result<Ruleset> {
        self.request(Method::GET, &format!("/repos/{owner}/{repo}/rulesets/{id}"))
            .send_json()
    }

    /// Create a ruleset.
    pub fn create_ruleset(&self, owner: &str, repo: &str, body: &Value) -> Result<()> {
        self.request(Method::POST, &format!("/repos/{owner}/{repo}/rulesets"))
            .json(body)
            .send_empty()
    }

    /// Replace a ruleset. The body is the full ruleset definition.
    pub fn update_ruleset(&self, owner: &str, repo: &str, id: u64, body: &Value) -> Result<()> {
        self.request(Method::PUT, &format!("/repos/{owner}/{repo}/rulesets/{id}"))
            .json(body)
            .send_empty()
    }

    // --- Actions ---------------------------------------------------------

    pub fn get_actions_permissions(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<ActionsPermissions>> {
        #[derive(Deserialize)]
        struct Body {
            enabled: bool,
            allowed_actions: Option<AllowedActions>,
        }
        let body = self
            .request(
                Method::GET,
                &format!("/repos/{owner}/{repo}/actions/permissions"),
            )
            .send_optional::<Body>()?;
        Ok(body.map(|body| ActionsPermissions {
            enabled: body.enabled,
            allowed_actions: body.allowed_actions.unwrap_or(AllowedActions::All),
        }))
    }

    pub fn set_actions_permissions(
        &self,
        owner: &str,
        repo: &str,
        perms: &ActionsPermissions,
    ) -> Result<()> {
        #[derive(Serialize)]
        struct Body {
            enabled: bool,
            allowed_actions: AllowedActions,
        }
        self.request(
            Method::PUT,
            &format!("/repos/{owner}/{repo}/actions/permissions"),
        )
        .json(&Body {
            enabled: perms.enabled,
            allowed_actions: perms.allowed_actions,
        })
        .send_empty()
    }

    pub fn get_workflow_permissions(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Option<WorkflowPermissions>> {
        self.request(
            Method::GET,
            &format!("/repos/{owner}/{repo}/actions/permissions/workflow"),
        )
        .send_optional::<WorkflowPermissions>()
    }

    pub fn set_workflow_permissions(
        &self,
        owner: &str,
        repo: &str,
        perms: &WorkflowPermissions,
    ) -> Result<()> {
        self.request(
            Method::PUT,
            &format!("/repos/{owner}/{repo}/actions/permissions/workflow"),
        )
        .json(perms)
        .send_empty()
    }

    /// Get the allow-list of actions.
    ///
    /// GitHub answers `409 Conflict` when the repository's actions policy is
    /// not `selected`; that simply means "no allow-list configured", so it is
    /// normalized to `None`.
    pub fn get_selected_actions(&self, owner: &str, repo: &str) -> Result<Option<SelectedActions>> {
        let path = format!("/repos/{owner}/{repo}/actions/permissions/selected-actions");
        let resp = self.request(Method::GET, &path).send()?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND || status == StatusCode::CONFLICT {
            return Ok(None);
        }
        parse_json(&format!("GET {path}"), resp).map(Some)
    }

    pub fn set_selected_actions(
        &self,
        owner: &str,
        repo: &str,
        selected: &SelectedActions,
    ) -> Result<()> {
        self.request(
            Method::PUT,
            &format!("/repos/{owner}/{repo}/actions/permissions/selected-actions"),
        )
        .json(selected)
        .send_empty()
    }
}
// ---------------------------------------------------------------------------
// Response parsing helpers
// ---------------------------------------------------------------------------

/// A prepared API call that remembers its method and path for error messages.
struct ApiRequest {
    builder: RequestBuilder,
    method: Method,
    path: String,
}

impl ApiRequest {
    fn context(&self) -> String {
        format!("{} {}", self.method.as_str(), self.path)
    }

    fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.builder = self.builder.json(body);
        self
    }

    fn send(self) -> Result<Response> {
        let context = self.context();
        self.builder.send().with_context(|| context)
    }

    fn send_json<T: DeserializeOwned>(self) -> Result<T> {
        let context = self.context();
        let resp = self.send()?;
        parse_json(&context, resp)
    }

    fn send_optional<T: DeserializeOwned>(self) -> Result<Option<T>> {
        let context = self.context();
        let resp = self.send()?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        parse_json(&context, resp).map(Some)
    }

    fn send_empty(self) -> Result<()> {
        let context = self.context();
        let resp = self.send()?;
        parse_empty(&context, resp)
    }
}

fn parse_json<T: DeserializeOwned>(context: &str, resp: Response) -> Result<T> {
    let status = resp.status();
    let text = resp.text().context("reading response body")?;
    if !status.is_success() {
        bail!(
            "{context}: GitHub API error ({}): {}",
            status.as_u16(),
            api_message(&text)
        );
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{context}: unexpected GitHub response: {text}"))
}

fn parse_empty(context: &str, resp: Response) -> Result<()> {
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().unwrap_or_default();
        bail!(
            "{context}: GitHub API error ({}): {}",
            status.as_u16(),
            api_message(&text)
        );
    }
    Ok(())
}

fn api_message(text: &str) -> String {
    let fallback = || text.trim().to_string();
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return fallback();
    };
    let Some(message) = value.get("message").and_then(Value::as_str) else {
        return fallback();
    };
    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
        let details: Vec<String> = errors.iter().map(ToString::to_string).collect();
        if !details.is_empty() {
            return format!("{message}: {}", details.join("; "));
        }
    }
    message.to_string()
}
