# nixit

Declare GitHub repository settings in Nix; `nixit` reconciles GitHub to match.

- **`nixit.lib`** — a small Nix library that validates each repository's
  settings (`nixit.lib.githubRepository`).
- **`nixit`** — a Rust CLI that creates missing repositories and applies every
  declared setting through the GitHub REST API.

Nix stays the source of truth; all network I/O happens in a testable Rust
program. There is no state file and no daemon: every run reads the live state
and the configuration, reports the difference, and (unless `--dry-run`) makes it
so.

## Quick start

Declare repositories in your flake (see
[Using it from another flake](#using-it-from-another-flake)):

```nix
githubRepositories.myrepo = nixit.lib.githubRepository {
  owner = "serephus";
  description = "My repository";
  features.wiki.enable = false;
  pull.merge.enable = false;
  pull.squash.enable = true;
  pull.rebase.enable = false;
  branch_protection.main.required_linear_history = true;
};
```

Then, with `$GITHUB_TOKEN` exported:

```console
$ nix run github:serephus/nixit -- --dry-run --flake .#myrepo  # preview
$ nix run github:serephus/nixit -- --flake .#myrepo            # apply
```

## Using it from another flake

```nix
{
  inputs.nixit.url = "github:serephus/nixit";

  outputs = { nixit, ... }: {
    githubRepositories.myrepo = nixit.lib.githubRepository {
      owner = "serephus";
      features.wiki.enable = false;
      pull.squash.enable = true;
    };
  };
}
```

`nixit.lib.githubRepository` validates a repository's settings; assign it to
`githubRepositories.<name>`. It fails evaluation on unknown options or invalid
enum values.

A complete, runnable downstream flake lives in
[`examples/flake.nix`](examples/flake.nix); [`examples/repos.nix`](examples/repos.nix)
shows every option.

## CLI

```text
nixit [--token <TOKEN>] --flake <REF> [-n|--dry-run] [-q]
```

`nixit` creates missing repositories (empty) and reconciles every setting.
Settings that need a branch are skipped while the repository is empty.
`--dry-run` (`-n`) prints the planned changes without touching GitHub.

The flake is given with `--flake <REF>`, e.g. `.#myrepo` (one repository) or `.#`
(every repository in `githubRepositories`).

### Authentication

The token is resolved from `--token`, then `$GITHUB_TOKEN`. Prefer the
environment variable: `--token` is visible in the process list and shell
history.

#### Required token permissions

`nixit` needs **admin** access to every repository it manages, so the token
owner must be an administrator of the repository. Branch protection on private
repositories additionally requires a paid plan (GitHub Pro/Team/Enterprise); it
is available on public repositories for free.

**Fine-grained personal access token.** Set repository access to **All
repositories** (required to create repositories; a token limited to selected
repositories can still reconcile existing ones) and grant these repository
permissions:

| Permission | Level | Used for |
| --- | --- | --- |
| **Administration** | Read and write | repository settings, topics, Actions permissions, branch protection, rulesets, and creating repositories |
| **Contents** | Read | checking whether the protected branch exists |
| **Metadata** | Read | reading repositories (included automatically) |

**Classic personal access token.** Grant the **`repo`** scope. It is the only
classic scope that covers repository administration (settings, topics, Actions,
and branch protection) for both public and private repositories.

`nixit` never deletes repositories, pushes commits, or edits workflow files, so
`delete_repo`, `workflow`, and code-write scopes are not needed.

## Configuration

Every option is optional. **An absent option means "leave it alone"; `nixit`
only enforces what you declare.** This is what makes partial configuration safe.

| Group | Options |
| --- | --- |
| Repository | `name`, `owner`, `description`, `homepage`, `topics`, `visibility` (`public` \| `private`) |
| Features | `features.{wiki,issues,projects,discussions}.enable` |
| State | `is_template`, `is_archived`, `allow_forking` |
| Pull requests | `pull.{merge,squash}.{enable,commit_title,commit_message}`, `pull.rebase.enable`, `pull.auto_merge`, `pull.delete_branch_on_merge`, `pull.update_branch`, `pull.web_commit_signoff_required` |
| Actions | `actions.enable`, `actions.policy` (`all` \| `local_only` \| `selected`), `actions.selected`, `actions.default_token_permissions` (`read` \| `write`), `actions.allow_pr_approval` |
| Branch protection | `branch_protection.<branch>.{...}` (see below) |
| Rulesets | `rulesets.<key>.{name,target,enforcement,conditions,bypass_actors,rules}` (see below) |

Constraints: at least one of `pull.{merge,squash,rebase}.enable` must be true, and
`pull.auto_merge = true` requires `pull.merge.enable` or `pull.squash.enable`.
`actions.policy = "selected"` requires `actions.selected`.

Commit text is declared per merge method. `pull.merge.commit_title` is one of
`pr_title` or `merge_message`, and `pull.merge.commit_message` is one of
`pr_body`, `pr_title`, or `blank`. `pull.squash.commit_title` is one of
`pr_title` or `commit_or_pr_title`, and `pull.squash.commit_message` is one of
`pr_body`, `commit_messages`, or `blank`. `pull.rebase` has no commit text
options. `allow_forking` only applies to public repositories.

`name` sets the GitHub repository name and defaults to the
`githubRepositories.<name>` attribute key.

### Branch protection

```nix
branch_protection.main = {
  required_linear_history = true;
  enforce_admins = true;
  allow_force_pushes = false;
  allow_deletions = false;
  block_creations = false;
  required_conversation_resolution = true;
  lock_branch = false;
  allow_fork_syncing = false;
  required_signatures = true;

  required_status_checks = {
    strict = true;
    contexts = [ "build" ];
  };

  required_pull_request_reviews = {
    dismiss_stale_reviews = true;
    require_code_owner_reviews = false;
    required_approving_review_count = 1;
    require_last_push_approval = true;
  };
};
```

### Rulesets

```nix
rulesets.main = {
  enforcement = "active"; # active | disabled | evaluate
  target = "branch"; # branch | tag | push

  conditions.ref_name = {
    include = [ "~DEFAULT_BRANCH" ];
    exclude = [ ];
  };

  bypass_actors = [
    {
      actor_id = 5; # omit for organization_admin and deploy_key
      actor_type = "repository_role"; # integration | organization_admin | repository_role | team | deploy_key | user
      bypass_mode = "always"; # always | pull_request | exempt
    }
  ];

  rules = [
    { type = "deletion"; }
    { type = "non_fast_forward"; }
    { type = "required_linear_history"; }
    { type = "required_signatures"; }
    {
      type = "pull_request";
      parameters = {
        required_approving_review_count = 1;
        dismiss_stale_reviews_on_push = true;
        require_code_owner_review = false;
        require_last_push_approval = false;
        required_review_thread_resolution = true;
      };
    }
    {
      type = "required_status_checks";
      parameters = {
        strict_required_status_checks_policy = true;
        required_status_checks = [ { context = "build"; } ];
      };
    }
  ];
};
```

Rulesets are matched to GitHub by `name`, which defaults to the configuration
attribute key. When a ruleset is declared, `rules` and `bypass_actors` replace
the live lists while `name`, `target`, `enforcement`, and `conditions` are
merged over the live ruleset; undeclared fields and rulesets are left alone.
Rule `parameters` are passed through to the GitHub API unchanged.

`rules[].type` must be one of GitHub's rule types (`creation`, `update`,
`deletion`, `required_linear_history`, `merge_queue`, `required_deployments`,
`required_signatures`, `pull_request`, `required_status_checks`,
`non_fast_forward`, `commit_message_pattern`, `commit_author_email_pattern`,
`committer_email_pattern`, `branch_name_pattern`, `tag_name_pattern`,
`workflows`, `code_scanning`, `code_quality`, `code_coverage`,
`copilot_code_review`, `license_compliance_scanning`, `file_path_restriction`,
`max_file_path_length`, `file_extension_restriction`, `max_file_size`).
`conditions.ref_name.include` also accepts `~DEFAULT_BRANCH` and `~ALL`.

## Empty repositories

Repositories are always created **empty** (no initial commit). Push the first
commit yourself, then run `nixit` again to apply branch protection, which GitHub
cannot apply before a branch exists.

## Development

```console
$ nix develop          # rust toolchain + rust-analyzer
$ cargo test           # unit + wiremock HTTP tests
$ cargo clippy --all-targets -- -D warnings
$ cargo fmt --check
$ nix flake check
$ nix build
```

The HTTP layer is tested against [`wiremock`](https://docs.rs/wiremock) and the
reconciliation logic is covered by unit tests, so the suite runs without a
GitHub token.

## License

GLWTPL
