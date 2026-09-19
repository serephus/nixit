# A complete downstream flake: declare GitHub repositories and manage them with
# nixit.
#
#   export GITHUB_TOKEN=ghp_...
#
#   nix run github:serephus/nixit -- --dry-run --flake .#myrepo
#   nix run github:serephus/nixit -- --flake .#myrepo
#
# `nixit.lib.githubRepository` validates a repository's settings; assign it to
# `githubRepositories.<name>`, which the CLI reads with `--flake`.
{
  description = "My GitHub repositories, managed by nixit";

  inputs.nixit.url = "github:serephus/nixit";

  outputs =
    { nixit, ... }:
    {
      # In your own flake, inline the repositories instead, for example:
      #
      #   githubRepositories.myrepo = nixit.lib.githubRepository {
      #     owner = "serephus";
      #     description = "My repository";
      #     features.wiki.enable = false;
      #     pull.merge.enable = false;
      #     rulesets.main = {
      #       enforcement = "active";
      #       rules = [ { type = "deletion"; } ];
      #     };
      #   };
      #
      # This example reuses the repository definitions next to it.
      githubRepositories = builtins.mapAttrs (_: nixit.lib.githubRepository) (import ./repos.nix).repos;
    };
}
