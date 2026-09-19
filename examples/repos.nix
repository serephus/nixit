# A complete `nixit` configuration.
#
# Every supported option is shown below. Delete whatever you do not need: an
# absent option is never touched, so partial configuration is safe.
#
# See `examples/flake.nix` for a complete downstream flake that consumes this
# file.
{
  repos = {
    full-example = {
      # --- Ownership ------------------------------------------------------
      # Repository name on GitHub. Defaults to the attribute key.
      name = "full-example";
      # Account that owns the repository. Falls back to the authenticated user.
      owner = "serephus";

      # --- Basic metadata -------------------------------------------------
      description = "Declare GitHub repository settings in Nix and sync them through the GitHub API";
      homepage = "https://github.com/serephus/nixit";
      topics = [
        "github"
        "nix"
        "rust"
        "configuration"
      ];
      visibility = "public"; # public | private

      # --- Feature toggles ------------------------------------------------
      features = {
        wiki.enable = false;
        issues.enable = true;
        projects.enable = false;
        discussions.enable = true;
      };

      # --- Repository state -----------------------------------------------
      is_template = false;
      is_archived = false;
      allow_forking = true; # public repositories only

      # --- Pull requests and merging --------------------------------------
      pull = {
        merge = {
          enable = false; # allow merge commits
          commit_title = "pr_title"; # pr_title | merge_message
          commit_message = "pr_body"; # pr_body | pr_title | blank
        };
        squash = {
          enable = true; # allow squash merges
          commit_title = "commit_or_pr_title"; # pr_title | commit_or_pr_title
          commit_message = "commit_messages"; # pr_body | commit_messages | blank
        };
        rebase.enable = false; # allow rebase merges
        auto_merge = true; # allow auto-merge
        delete_branch_on_merge = true;
        update_branch = true;
        web_commit_signoff_required = false;
      };

      # --- GitHub Actions -------------------------------------------------
      actions = {
        enable = true;
        policy = "selected"; # all | local_only | selected
        selected = {
          github_owned = true;
          verified = false;
          patterns = [
            "actions/*"
            "Swatinem/rust-cache@*"
          ];
        };
        default_token_permissions = "read"; # read | write
        allow_pr_approval = false;
      };

      # --- Branch protection ----------------------------------------------
      # Keyed by branch name. Skipped (with a warning) while the branch does
      # not exist yet, e.g. on an empty repository. Run `nixit` after pushing
      # the first commit to apply it.
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
          contexts = [
            # this should be the names of github action jobs
            # see .github/workflows/dev.yml
            # ${{ matrix.os }}-${{ matrix.target }}-${{ matrix.toolchain }}
            "ubuntu-latest-x86_64-unknown-linux-gnu-nightly"
            "ubuntu-latest-x86_64-unknown-linux-gnu-stable"
            "Nix Build"
          ];
        };

        required_pull_request_reviews = {
          dismiss_stale_reviews = true;
          require_code_owner_reviews = false;
          required_approving_review_count = 1;
          require_last_push_approval = true;
        };
      };

      # --- Rulesets -------------------------------------------------------
      # Rulesets are matched to GitHub by `name` (defaulting to the attribute
      # key). `rules` and `bypass_actors` replace the live lists; the other
      # fields are merged over the live ruleset.
      rulesets.main = {
        enforcement = "active"; # active | disabled | evaluate
        target = "branch"; # branch | tag | push

        conditions.ref_name = {
          include = [ "~DEFAULT_BRANCH" ];
          exclude = [ ];
        };

        bypass_actors = [
          {
            actor_id = 5; # repository role id; omit for organization_admin/deploy_key
            actor_type = "repository_role"; # integration | organization_admin | repository_role | team | deploy_key | user
            bypass_mode = "always"; # always | pull_request | exempt
          }
        ];

        # Rule parameters are passed through to the GitHub API unchanged.
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
              required_status_checks = [
                { context = "Nix Build"; }
              ];
            };
          }
        ];
      };
    };

    # A second repository, kept deliberately minimal, to show that the schema
    # is just a set of repositories.
    sandbox = {
      owner = "serephus";
      description = "Scratch space";
      visibility = "private";
      features.wiki.enable = false;
    };
  };
}
