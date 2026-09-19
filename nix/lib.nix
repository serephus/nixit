let
  inherit (builtins)
    all
    attrNames
    concatMap
    concatStringsSep
    elem
    filter
    isAttrs
    isList
    isNull
    listToAttrs
    toString
    ;

  # Recursively drop `null` attributes so that an option that is not set is
  # simply absent from the output. The CLI treats absent as "leave alone".
  stripNulls =
    value:
    if isAttrs value then
      listToAttrs (
        concatMap (
          name:
          let
            stripped = stripNulls value.${name};
          in
          if isNull stripped then
            [ ]
          else
            [
              {
                inherit name;
                value = stripped;
              }
            ]
        ) (attrNames value)
      )
    else if isList value then
      map stripNulls value
    else
      value;

  repoKeys = [
    "name"
    "owner"
    "description"
    "homepage"
    "topics"
    "visibility"
    "features"
    "is_template"
    "is_archived"
    "allow_forking"
    "pull"
    "actions"
    "branch_protection"
    "rulesets"
  ];

  toggleKeys = [ "enable" ];

  featureKeys = [
    "wiki"
    "issues"
    "projects"
    "discussions"
  ];

  prKeys = [
    "merge"
    "squash"
    "rebase"
    "auto_merge"
    "delete_branch_on_merge"
    "update_branch"
    "web_commit_signoff_required"
  ];

  # Merge and squash accept commit title/message; rebase has no such options.
  mergeMethodKeys = [
    "enable"
    "commit_title"
    "commit_message"
  ];

  rebaseKeys = [ "enable" ];

  mergeCommitTitles = [
    "pr_title"
    "merge_message"
  ];

  mergeCommitMessages = [
    "pr_body"
    "pr_title"
    "blank"
  ];

  squashCommitTitles = [
    "pr_title"
    "commit_or_pr_title"
  ];

  squashCommitMessages = [
    "pr_body"
    "commit_messages"
    "blank"
  ];

  actionsKeys = [
    "enable"
    "policy"
    "selected"
    "default_token_permissions"
    "allow_pr_approval"
  ];

  selectedActionsKeys = [
    "github_owned"
    "verified"
    "patterns"
  ];

  branchKeys = [
    "required_linear_history"
    "enforce_admins"
    "allow_force_pushes"
    "allow_deletions"
    "block_creations"
    "required_conversation_resolution"
    "lock_branch"
    "allow_fork_syncing"
    "required_signatures"
    "required_status_checks"
    "required_pull_request_reviews"
  ];

  statusCheckKeys = [
    "strict"
    "contexts"
  ];

  prReviewKeys = [
    "dismiss_stale_reviews"
    "require_code_owner_reviews"
    "required_approving_review_count"
    "require_last_push_approval"
  ];

  rulesetKeys = [
    "name"
    "target"
    "enforcement"
    "conditions"
    "bypass_actors"
    "rules"
  ];

  rulesetTargets = [
    "branch"
    "tag"
    "push"
  ];

  enforcements = [
    "disabled"
    "active"
    "evaluate"
  ];

  conditionKeys = [ "ref_name" ];

  refNameKeys = [
    "include"
    "exclude"
  ];

  bypassActorKeys = [
    "actor_id"
    "actor_type"
    "bypass_mode"
  ];

  bypassActorTypes = [
    "integration"
    "organization_admin"
    "repository_role"
    "team"
    "deploy_key"
    "user"
  ];

  bypassModes = [
    "always"
    "pull_request"
    "exempt"
  ];

  ruleKeys = [
    "type"
    "parameters"
  ];

  ruleTypes = [
    "creation"
    "update"
    "deletion"
    "required_linear_history"
    "merge_queue"
    "required_deployments"
    "required_signatures"
    "pull_request"
    "required_status_checks"
    "non_fast_forward"
    "commit_message_pattern"
    "commit_author_email_pattern"
    "committer_email_pattern"
    "branch_name_pattern"
    "tag_name_pattern"
    "workflows"
    "code_scanning"
    "code_quality"
    "code_coverage"
    "copilot_code_review"
    "license_compliance_scanning"
    "file_path_restriction"
    "max_file_path_length"
    "file_extension_restriction"
    "max_file_size"
  ];

  validTopic =
    topic:
    builtins.isString topic
    && builtins.stringLength topic >= 1
    && builtins.stringLength topic <= 50
    && builtins.match "[A-Za-z0-9._-]+" topic != null;

  checkTopics =
    topics:
    if all validTopic topics then
      true
    else
      throw "nixit: invalid topic (topics are 1-50 characters of [A-Za-z0-9._-])";

  checkKeys =
    context: allowed: attrs:
    let
      unknown = filter (key: !(elem key allowed)) (attrNames attrs);
    in
    if unknown == [ ] then
      true
    else
      throw "nixit: unknown option(s) in ${context}: ${concatStringsSep ", " unknown}";

  checkEnum =
    context: allowed: value:
    if elem value allowed then
      true
    else
      throw "nixit: invalid value `${toString value}` in ${context}; expected one of ${concatStringsSep ", " allowed}";

  checkStringList =
    context: value:
    if isList value && all builtins.isString value then
      true
    else
      throw "nixit: ${context} must be a list of strings";

  validateRepo =
    cfg:
    let
      features = cfg.features or { };
      pull = cfg.pull or { };
      actions = cfg.actions or { };
      branches = cfg.branch_protection or { };
      rulesets = cfg.rulesets or { };
      mergeMethods = [
        (pull.merge.enable or null)
        (pull.squash.enable or null)
        (pull.rebase.enable or null)
      ];
      featureOk =
        feature:
        if features ? ${feature} then
          checkKeys "features.${feature}" toggleKeys features.${feature}
        else
          true;
      mergeMethodOk =
        method: keys: titles: messages:
        if pull ? ${method} then
          checkKeys "pull.${method}" keys pull.${method}
          && (
            if pull.${method} ? commit_title then
              checkEnum "pull.${method}.commit_title" titles pull.${method}.commit_title
            else
              true
          )
          && (
            if pull.${method} ? commit_message then
              checkEnum "pull.${method}.commit_message" messages pull.${method}.commit_message
            else
              true
          )
        else
          true;
      checkBypassActor =
        key: actor:
        checkKeys "rulesets.${key}.bypass_actors" bypassActorKeys actor
        && (
          if actor ? actor_type then
            checkEnum "rulesets.${key}.bypass_actors.actor_type" bypassActorTypes actor.actor_type
          else
            throw "nixit: rulesets.${key}.bypass_actors requires actor_type"
        )
        && (
          if actor ? bypass_mode then
            checkEnum "rulesets.${key}.bypass_actors.bypass_mode" bypassModes actor.bypass_mode
          else
            true
        );
      checkRule =
        key: rule:
        checkKeys "rulesets.${key}.rules" ruleKeys rule
        && (
          if rule ? type then
            checkEnum "rulesets.${key}.rules.type" ruleTypes rule.type
          else
            throw "nixit: rulesets.${key}.rules requires type"
        )
        && (
          if rule ? parameters then
            if isAttrs rule.parameters then
              true
            else
              throw "nixit: rulesets.${key}.rules.parameters must be an attribute set"
          else
            true
        );
      checkRuleset =
        key: ruleset:
        checkKeys "rulesets.${key}" rulesetKeys ruleset
        && (
          if ruleset ? target then checkEnum "rulesets.${key}.target" rulesetTargets ruleset.target else true
        )
        && (
          if ruleset ? enforcement then
            checkEnum "rulesets.${key}.enforcement" enforcements ruleset.enforcement
          else
            true
        )
        && (
          if ruleset ? conditions then
            checkKeys "rulesets.${key}.conditions" conditionKeys ruleset.conditions
            && (
              if ruleset.conditions ? ref_name then
                checkKeys "rulesets.${key}.conditions.ref_name" refNameKeys ruleset.conditions.ref_name
                && (
                  if ruleset.conditions.ref_name ? include then
                    checkStringList "rulesets.${key}.conditions.ref_name.include" ruleset.conditions.ref_name.include
                  else
                    true
                )
                && (
                  if ruleset.conditions.ref_name ? exclude then
                    checkStringList "rulesets.${key}.conditions.ref_name.exclude" ruleset.conditions.ref_name.exclude
                  else
                    true
                )
              else
                true
            )
          else
            true
        )
        && (if ruleset ? bypass_actors then all (checkBypassActor key) ruleset.bypass_actors else true)
        && (if ruleset ? rules then all (checkRule key) ruleset.rules else true);
    in
    checkKeys "repository" repoKeys cfg
    && (
      if cfg ? visibility then
        checkEnum "visibility" [ "public" "private" "internal" ] cfg.visibility
      else
        true
    )
    && (if cfg ? topics then checkTopics cfg.topics else true)
    && (
      if cfg ? features then
        checkKeys "features" featureKeys features && all featureOk featureKeys
      else
        true
    )
    && (
      if cfg ? pull then
        checkKeys "pull" prKeys pull
        && mergeMethodOk "merge" mergeMethodKeys mergeCommitTitles mergeCommitMessages
        && mergeMethodOk "squash" mergeMethodKeys squashCommitTitles squashCommitMessages
        && mergeMethodOk "rebase" rebaseKeys [ ] [ ]
        && (
          if all (method: method == false) mergeMethods then
            throw "nixit: at least one of pull.merge.enable, pull.squash.enable, or pull.rebase.enable must be true"
          else
            true
        )
        && (
          if (pull.auto_merge or false) && !(elem true mergeMethods) then
            throw "nixit: pull.auto_merge = true requires pull.merge.enable or pull.squash.enable"
          else
            true
        )
      else
        true
    )
    && (
      if cfg ? actions then
        checkKeys "actions" actionsKeys actions
        && (
          if actions ? policy then
            checkEnum "actions.policy" [
              "all"
              "local_only"
              "selected"
            ] actions.policy
          else
            true
        )
        && (
          if actions ? selected then
            checkKeys "actions.selected" selectedActionsKeys actions.selected
          else
            true
        )
        && (
          if actions ? default_token_permissions then
            checkEnum "actions.default_token_permissions" [
              "read"
              "write"
            ] actions.default_token_permissions
          else
            true
        )
        && (
          if (actions.policy or null) == "selected" && !(actions ? selected) then
            throw "nixit: actions.policy = \"selected\" also requires actions.selected"
          else
            true
        )
      else
        true
    )
    && (
      if cfg ? branch_protection then
        all (
          branch:
          let
            bc = branches.${branch};
          in
          checkKeys "branch_protection.${branch}" branchKeys bc
          && (
            if bc ? required_status_checks then
              checkKeys "branch_protection.${branch}.required_status_checks" statusCheckKeys
                bc.required_status_checks
            else
              true
          )
          && (
            if bc ? required_pull_request_reviews then
              checkKeys "branch_protection.${branch}.required_pull_request_reviews" prReviewKeys
                bc.required_pull_request_reviews
            else
              true
          )
        ) (attrNames branches)
      else
        true
    )
    && (
      if cfg ? rulesets then all (key: checkRuleset key rulesets.${key}) (attrNames rulesets) else true
    );

in
{
  # Validate and normalise a single repository's settings, ready to assign to
  # `githubRepositories.<name>`:
  #
  #   outputs = { self, nixit, ... }: {
  #     githubRepositories.myrepo = nixit.lib.githubRepository {
  #       owner = "me";
  #       features.wiki.enable = false;
  #     };
  #   };
  #
  # The CLI reads `githubRepositories` directly with `--flake .#myrepo`.
  githubRepository =
    cfg:
    assert validateRepo cfg;
    stripNulls cfg;
}
