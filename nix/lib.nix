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
    "pull"
    "actions"
    "branch_protection"
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
  ];

  mergeKeys = [ "enable" ];

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

  validateRepo =
    cfg:
    let
      features = cfg.features or { };
      pull = cfg.pull or { };
      actions = cfg.actions or { };
      branches = cfg.branch_protection or { };
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
      mergeOk =
        method: if pull ? ${method} then checkKeys "pull.${method}" mergeKeys pull.${method} else true;
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
        && all mergeOk [
          "merge"
          "squash"
          "rebase"
        ]
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
