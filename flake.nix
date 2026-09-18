{
  description = "Declare GitHub repository settings in Nix and sync them through the GitHub API";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      rust-overlay,
      naersk,
      ...
    }:
    let
      lib = import ./nix/lib.nix;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
    in
    {
      # Downstream flakes use `nixit.lib.githubRepository`.
      inherit lib;

      # `nixit`'s own repository settings (read by `--flake .#nixit`).
      githubRepositories.nixit = lib.githubRepository {
        owner = "serephus";
        name = "nixit";

        description = "Declare GitHub repository settings in Nix and sync them through the GitHub API";
        homepage = "https://github.com/serephus/nixit";
        topics = [
          "github"
          "nix"
          "rust"
          "configuration"
        ];
        visibility = "public";

        features = {
          wiki.enable = false;
          issues.enable = true;
          projects.enable = false;
          discussions.enable = false;
        };

        is_template = false;
        is_archived = false;

        pull = {
          merge.enable = true;
          squash.enable = false;
          rebase.enable = false;
          auto_merge = true;
          delete_branch_on_merge = true;
          update_branch = true;
        };

        actions = {
          enable = true;
          policy = "all";
          default_token_permissions = "read";
          allow_pr_approval = false;
        };

        branch_protection.main = {
          allow_force_pushes = false;
          allow_deletions = false;
          required_conversation_resolution = true;

          required_status_checks = {
            strict = true;
            contexts = [
              "ubuntu-latest-x86_64-unknown-linux-gnu-nightly"
              "ubuntu-latest-x86_64-unknown-linux-gnu-stable"
              "Nix Build"
            ];
          };
        };
      };
    }
    // flake-utils.lib.eachSystem systems (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        };
        naersk' = pkgs.callPackage naersk { };
      in
      {
        packages = rec {
          default = nixit;
          nixit = naersk'.buildPackage {
            src = ./.;
            nativeBuildInputs = [ pkgs.pkg-config ];
          };
        };

        devShells.default = pkgs.mkShell {
          name = "nixit";
          packages = [
            rust
            pkgs.pkg-config
            pkgs.stdenv.cc
          ];
        };

        formatter = pkgs.nixfmt;
      }
    );
}
