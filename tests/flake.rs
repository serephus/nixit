//! Tests for reading configuration from a flake's `githubRepositories` output.
//!
//! These shell out to `nix`, so they are skipped when Nix is not on `PATH`
//! (for example in the plain `cargo` CI job).

use nixit::config::Config;

fn nix_available() -> bool {
    std::process::Command::new("nix")
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn loads_repos_from_a_flake_output() {
    if !nix_available() {
        eprintln!("skipping: `nix` is not on PATH");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("flake.nix"),
        r#"
{
  outputs = { self }: {
    githubRepositories = {
      "my-repo" = { owner = "octocat"; features.wiki.enable = false; pull.squash.enable = true; };
      other = { owner = "hubot"; description = "hi"; };
    };
  };
}
"#,
    )
    .unwrap();

    let base = dir.path().canonicalize().unwrap();
    let single = Config::from_flake(&format!("{}#my-repo", base.display())).unwrap();
    assert_eq!(single.repos["my-repo"].owner.as_deref(), Some("octocat"));
    assert_eq!(single.repos.len(), 1);
    assert_eq!(
        single.repos["my-repo"]
            .features
            .as_ref()
            .unwrap()
            .wiki
            .as_ref()
            .unwrap()
            .enable,
        Some(false)
    );

    let all = Config::from_flake(&base.to_string_lossy()).unwrap();
    assert_eq!(all.repos.len(), 2);
    assert_eq!(all.repos["other"].owner.as_deref(), Some("hubot"));
}
