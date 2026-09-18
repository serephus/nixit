//! Thin wrapper around the GitHub REST API (github.com only).
//!
//! We talk to the raw REST endpoints rather than using a high-level client
//! because several of the endpoints we need (Actions permissions and branch
//! protection) are not consistently modelled by existing crates.

use anyhow::{Context, Result, bail};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::Method;
use reqwest::StatusCode;
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{AllowedActions, RequiredPullRequestReviews as RprConfig, WorkflowPermission};

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
    pub is_template: Option<bool>,
    pub archived: Option<bool>,
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

/// Canonical, fully-materialised branch protection, ready to `PUT`.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct BranchProtection {
    pub required_status_checks: Option<RequiredStatusChecks>,
    pub enforce_admins: bool,
    pub required_pull_request_reviews: Option<RequiredPullRequestReviews>,
    pub restrictions: Option<Restrictions>,
    pub required_linear_history: bool,
    pub allow_force_pushes: bool,
    pub allow_deletions: bool,
    pub block_creations: bool,
    pub required_conversation_resolution: bool,
    pub lock_branch: bool,
    pub allow_fork_syncing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequiredStatusChecks {
    pub strict: bool,
    pub contexts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequiredPullRequestReviews {
    pub dismiss_stale_reviews: bool,
    pub require_code_owner_reviews: bool,
    pub required_approving_review_count: u32,
    pub require_last_push_approval: bool,
}

/// Branch restrictions. Round-tripped but not configurable.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Restrictions {
    pub users: Vec<String>,
    pub teams: Vec<String>,
    pub apps: Vec<String>,
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

    // --- Branch protection ----------------------------------------------

    pub fn get_branch_protection(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<BranchProtection>> {
        let branch = encode_segment(branch);
        let body = self
            .request(
                Method::GET,
                &format!("/repos/{owner}/{repo}/branches/{branch}/protection"),
            )
            .send_optional::<BranchProtectionResponse>()?;
        Ok(body.map(Into::into))
    }

    pub fn set_branch_protection(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        protection: &BranchProtection,
    ) -> Result<()> {
        let branch = encode_segment(branch);
        self.request(
            Method::PUT,
            &format!("/repos/{owner}/{repo}/branches/{branch}/protection"),
        )
        .json(protection)
        .send_empty()
    }

    pub fn get_required_signatures(&self, owner: &str, repo: &str, branch: &str) -> Result<bool> {
        #[derive(Deserialize)]
        struct Body {
            #[serde(default)]
            enabled: bool,
        }
        let branch = encode_segment(branch);
        let body = self
            .request(
                Method::GET,
                &format!("/repos/{owner}/{repo}/branches/{branch}/protection/required_signatures"),
            )
            .send_optional::<Body>()?;
        Ok(body.map(|body| body.enabled).unwrap_or(false))
    }

    pub fn set_required_signatures(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
        enabled: bool,
    ) -> Result<()> {
        let branch = encode_segment(branch);
        let path =
            format!("/repos/{owner}/{repo}/branches/{branch}/protection/required_signatures");
        let method = if enabled {
            Method::POST
        } else {
            Method::DELETE
        };
        self.request(method, &path).send_empty()
    }

    /// Whether a branch exists in the repository.
    pub fn branch_exists(&self, owner: &str, repo: &str, branch: &str) -> Result<bool> {
        let branch = encode_segment(branch);
        let path = format!("/repos/{owner}/{repo}/branches/{branch}");
        let resp = self.request(Method::GET, &path).send()?;
        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            bail!(
                "GET {path}: GitHub API error ({}): {}",
                status.as_u16(),
                api_message(&text)
            );
        }
        Ok(true)
    }
}

fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, NON_ALPHANUMERIC).to_string()
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

// --- Branch protection response decoding -----------------------------------

#[derive(Debug, Deserialize)]
struct EnabledResponse {
    #[serde(default)]
    enabled: bool,
}

#[derive(Debug, Deserialize)]
struct RscResponse {
    #[serde(default)]
    strict: bool,
    #[serde(default)]
    contexts: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RprResponse {
    #[serde(default)]
    dismiss_stale_reviews: bool,
    #[serde(default)]
    require_code_owner_reviews: bool,
    #[serde(default)]
    required_approving_review_count: u32,
    #[serde(default)]
    require_last_push_approval: bool,
}

#[derive(Debug, Deserialize)]
struct RestrictionsResponse {
    #[serde(default)]
    users: Vec<UserRef>,
    #[serde(default)]
    teams: Vec<TeamRef>,
    #[serde(default)]
    apps: Vec<AppRef>,
}

#[derive(Debug, Deserialize)]
struct UserRef {
    login: String,
}

#[derive(Debug, Deserialize)]
struct TeamRef {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct AppRef {
    slug: String,
}

#[derive(Debug, Deserialize)]
struct BranchProtectionResponse {
    #[serde(default)]
    required_status_checks: Option<RscResponse>,
    #[serde(default)]
    enforce_admins: Option<EnabledResponse>,
    #[serde(default)]
    required_pull_request_reviews: Option<RprResponse>,
    #[serde(default)]
    restrictions: Option<RestrictionsResponse>,
    #[serde(default)]
    required_linear_history: Option<EnabledResponse>,
    #[serde(default)]
    allow_force_pushes: Option<EnabledResponse>,
    #[serde(default)]
    allow_deletions: Option<EnabledResponse>,
    #[serde(default)]
    block_creations: Option<EnabledResponse>,
    #[serde(default)]
    required_conversation_resolution: Option<EnabledResponse>,
    #[serde(default)]
    lock_branch: Option<EnabledResponse>,
    #[serde(default)]
    allow_fork_syncing: Option<EnabledResponse>,
}

impl From<BranchProtectionResponse> for BranchProtection {
    fn from(value: BranchProtectionResponse) -> Self {
        fn enabled(value: Option<EnabledResponse>) -> bool {
            value.map(|e| e.enabled).unwrap_or(false)
        }
        BranchProtection {
            required_status_checks: value
                .required_status_checks
                .map(|rsc| RequiredStatusChecks {
                    strict: rsc.strict,
                    contexts: rsc.contexts,
                }),
            enforce_admins: enabled(value.enforce_admins),
            required_pull_request_reviews: value.required_pull_request_reviews.map(|rpr| {
                RequiredPullRequestReviews {
                    dismiss_stale_reviews: rpr.dismiss_stale_reviews,
                    require_code_owner_reviews: rpr.require_code_owner_reviews,
                    required_approving_review_count: rpr.required_approving_review_count,
                    require_last_push_approval: rpr.require_last_push_approval,
                }
            }),
            restrictions: value.restrictions.map(|r| Restrictions {
                users: r.users.into_iter().map(|u| u.login).collect(),
                teams: r.teams.into_iter().map(|t| t.slug).collect(),
                apps: r.apps.into_iter().map(|a| a.slug).collect(),
            }),
            required_linear_history: enabled(value.required_linear_history),
            allow_force_pushes: enabled(value.allow_force_pushes),
            allow_deletions: enabled(value.allow_deletions),
            block_creations: enabled(value.block_creations),
            required_conversation_resolution: enabled(value.required_conversation_resolution),
            lock_branch: enabled(value.lock_branch),
            allow_fork_syncing: enabled(value.allow_fork_syncing),
        }
    }
}

/// Assemble a canonical branch protection from the current state plus the
/// declared (partial) config. This is what makes a partial `required_linear_history`
/// declaration safe: the full object is reconstructed before the `PUT`.
pub fn merge_branch_protection(
    current: &BranchProtection,
    config: &crate::config::BranchProtectionConfig,
) -> BranchProtection {
    let rs = match &config.required_status_checks {
        Some(cfg) => {
            let base = current.required_status_checks.as_ref();
            Some(RequiredStatusChecks {
                strict: cfg
                    .strict
                    .or_else(|| base.map(|b| b.strict))
                    .unwrap_or(false),
                contexts: cfg
                    .contexts
                    .clone()
                    .or_else(|| base.map(|b| b.contexts.clone()))
                    .unwrap_or_default(),
            })
        }
        None => current.required_status_checks.clone(),
    };
    let rpr = match &config.required_pull_request_reviews {
        Some(cfg) => {
            let base = current.required_pull_request_reviews.as_ref();
            Some(merge_rpr(base, cfg))
        }
        None => current.required_pull_request_reviews.clone(),
    };
    BranchProtection {
        required_status_checks: rs,
        enforce_admins: config.enforce_admins.unwrap_or(current.enforce_admins),
        required_pull_request_reviews: rpr,
        restrictions: current.restrictions.clone(),
        required_linear_history: config
            .required_linear_history
            .unwrap_or(current.required_linear_history),
        allow_force_pushes: config
            .allow_force_pushes
            .unwrap_or(current.allow_force_pushes),
        allow_deletions: config.allow_deletions.unwrap_or(current.allow_deletions),
        block_creations: config.block_creations.unwrap_or(current.block_creations),
        required_conversation_resolution: config
            .required_conversation_resolution
            .unwrap_or(current.required_conversation_resolution),
        lock_branch: config.lock_branch.unwrap_or(current.lock_branch),
        allow_fork_syncing: config
            .allow_fork_syncing
            .unwrap_or(current.allow_fork_syncing),
    }
}

fn merge_rpr(
    base: Option<&RequiredPullRequestReviews>,
    cfg: &RprConfig,
) -> RequiredPullRequestReviews {
    RequiredPullRequestReviews {
        dismiss_stale_reviews: cfg
            .dismiss_stale_reviews
            .or_else(|| base.map(|b| b.dismiss_stale_reviews))
            .unwrap_or(false),
        require_code_owner_reviews: cfg
            .require_code_owner_reviews
            .or_else(|| base.map(|b| b.require_code_owner_reviews))
            .unwrap_or(false),
        required_approving_review_count: cfg
            .required_approving_review_count
            .or_else(|| base.map(|b| b.required_approving_review_count))
            .unwrap_or(1),
        require_last_push_approval: cfg
            .require_last_push_approval
            .or_else(|| base.map(|b| b.require_last_push_approval))
            .unwrap_or(false),
    }
}
