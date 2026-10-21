//! Free cloud machines: a GitHub codespace reached through `gh`'s SSH relay.
//! The codespace's OpenSSH config lives in MonoCode's data directory, so the
//! user's own `~/.ssh/config` is never edited.
use crate::remote_ssh::{Job, SshTarget};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{atomic::Ordering, Arc};
use std::time::{Duration, Instant};

/// 2 cores and 8 GB: the smallest machine, so free hours last longest.
const MACHINE: &str = "basicLinux32gb";
/// GitHub's maximum. Agents keep running this long after the last connection.
const IDLE_TIMEOUT: &str = "240m";
const SCOPE_HELP: &str = "GitHub has not allowed MonoCode to manage codespaces yet. Run `gh auth refresh -h github.com -s codespace` in a terminal, approve it in the browser, then try again.";

pub fn validate_repo(repo: &str) -> Result<String, String> {
    let repo = repo.trim().trim_end_matches(".git");
    let repo = repo
        .strip_prefix("https://github.com/")
        .unwrap_or(repo)
        .trim_end_matches('/');
    let safe = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part.len() <= 100
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    match repo.split_once('/') {
        Some((owner, name)) if safe(owner) && safe(name) => Ok(format!("{owner}/{name}")),
        _ => Err("Enter a GitHub repository as owner/name, such as evxnche/monocode.".into()),
    }
}

pub fn validate_branch(branch: &str) -> Result<Option<String>, String> {
    let branch = branch.trim();
    if branch.is_empty() {
        return Ok(None);
    }
    if branch.starts_with('-')
        || branch.len() > 200
        || !branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
    {
        return Err("Enter a branch name such as main, or leave it empty.".into());
    }
    Ok(Some(branch.into()))
}

/// The `Host` alias `gh codespace ssh --config` wrote for the codespace.
pub fn host_alias(config: &str) -> Result<String, String> {
    config
        .lines()
        .find_map(|line| line.trim().strip_prefix("Host "))
        .map(str::trim)
        .filter(|alias| crate::remote_ssh::validate_target(alias, None).is_ok())
        .map(str::to_string)
        .ok_or_else(|| "gh did not return SSH settings for the codespace".into())
}

fn is_github_cli(version: &str) -> bool {
    version.trim_start().starts_with("gh version ")
}

/// The GitHub CLI, skipping other programs named `gh` (npm's node-gh, say).
fn github_cli() -> Option<PathBuf> {
    let name = if cfg!(windows) { "gh.exe" } else { "gh" };
    std::env::split_paths(&crate::harness::gui_search_path())
        .map(|dir| dir.join(name))
        .filter(|path| path.is_file())
        .find(|path| {
            let mut cmd = Command::new(path);
            cmd.arg("--version")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            crate::hide_window_console(&mut cmd);
            let Ok(mut child) = cmd.spawn() else {
                return false;
            };
            // Some other `gh` programs wait for input instead of answering.
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(child.try_wait(), Ok(None)) {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            child
                .wait_with_output()
                .map(|output| {
                    output.status.success()
                        && is_github_cli(&String::from_utf8_lossy(&output.stdout))
                })
                .unwrap_or(false)
        })
}

fn gh(program: &Path, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("NO_COLOR", "1");
    crate::harness::apply_gui_env(&mut cmd);
    crate::hide_window_console(&mut cmd);
    let output = cmd
        .output()
        .map_err(|e| format!("Could not run gh: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("codespace\" scope") || stderr.contains("-s codespace") {
        return Err(SCOPE_HELP.into());
    }
    let reason = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .unwrap_or("unknown error");
    Err(format!("GitHub Codespaces: {reason}"))
}

/// Create a codespace on `repo`, wait for it, and return an SSH target for it.
pub fn create(
    job: &Arc<Job>,
    config_dir: &Path,
    repo: &str,
    branch: Option<&str>,
) -> Result<SshTarget, String> {
    let program = github_cli().ok_or(
        "GitHub CLI (gh) is not installed. Install it from https://cli.github.com and run `gh auth login`.",
    )?;
    job.message("Checking GitHub Codespaces access…");
    gh(&program, &["codespace", "list", "--limit", "1"])?;
    job.message("Creating a free GitHub codespace… this takes a minute or two");
    let display = format!("MonoCode {}", repo.rsplit('/').next().unwrap_or(repo));
    let display: String = display.chars().take(48).collect();
    let mut args = vec![
        "codespace",
        "create",
        "--repo",
        repo,
        "--machine",
        MACHINE,
        "--idle-timeout",
        IDLE_TIMEOUT,
        "--default-permissions",
        "--display-name",
        &display,
    ];
    if let Some(branch) = branch {
        args.extend(["--branch", branch]);
    }
    let created = gh(&program, &args)?;
    let name = created
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .last()
        .filter(|name| name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        .ok_or("gh did not report the new codespace's name")?
        .to_string();
    job.message("Waiting for the codespace to start…");
    let started = Instant::now();
    loop {
        if job.cancelled.load(Ordering::Relaxed) {
            return Err(format!(
                "Connection cancelled. The codespace {name} was created; delete it at github.com/codespaces if you do not need it."
            ));
        }
        let state = gh(
            &program,
            &["codespace", "view", "--codespace", &name, "--json", "state", "--jq", ".state"],
        )?;
        if state == "Available" {
            break;
        }
        if started.elapsed() > Duration::from_secs(600) {
            return Err(format!("The codespace {name} did not start within 10 minutes (state: {state})."));
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    job.message("Preparing SSH access to the codespace…");
    let config = gh(&program, &["codespace", "ssh", "--config", "--codespace", &name])?;
    let alias = host_alias(&config)?;
    std::fs::create_dir_all(config_dir).map_err(|e| e.to_string())?;
    let path: PathBuf = config_dir.join(format!("{name}.ssh_config"));
    std::fs::write(&path, format!("{config}\n")).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(SshTarget {
        target: alias,
        port: None,
        remote_port: 3774,
        config_file: Some(path.to_string_lossy().into_owned()),
        codespace: Some(name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_github_repositories_and_rejects_options() {
        assert_eq!(validate_repo("evxnche/monocode").unwrap(), "evxnche/monocode");
        assert_eq!(
            validate_repo("https://github.com/evxnche/monocode.git").unwrap(),
            "evxnche/monocode"
        );
        for bad in ["", "monocode", "-R/x", "a/b/c", "a/b;rm", "a/--web"] {
            assert!(validate_repo(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_branch("").unwrap(), None);
        assert_eq!(validate_branch("feat/x").unwrap().as_deref(), Some("feat/x"));
        assert!(validate_branch("--web").is_err());
        assert!(validate_branch("a b").is_err());
    }

    #[test]
    fn tells_the_github_cli_from_other_gh_programs() {
        assert!(is_github_cli("gh version 2.83.0 (2026-09-30)\nhttps://github.com/cli/cli"));
        assert!(!is_github_cli("0.1.4"));
        assert!(!is_github_cli("usage: gh <command> [--flags]"));
    }

    #[test]
    #[ignore = "needs the GitHub CLI installed on this machine"]
    fn finds_the_installed_github_cli() {
        let program = github_cli().expect("GitHub CLI not found");
        let output = Command::new(&program).arg("--version").output().unwrap();
        assert!(is_github_cli(&String::from_utf8_lossy(&output.stdout)), "{program:?}");
    }

    #[test]
    fn reads_the_host_alias_gh_wrote() {
        let config = "Host cs.fuzzy-space-abc123.main\n\tUser codespace\n\tProxyCommand /opt/homebrew/bin/gh cs ssh -c fuzzy-space-abc123 --stdio\n";
        assert_eq!(host_alias(config).unwrap(), "cs.fuzzy-space-abc123.main");
        assert!(host_alias("").is_err());
        assert!(host_alias("Host -oProxyCommand=x").is_err());
    }
}
