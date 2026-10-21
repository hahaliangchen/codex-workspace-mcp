//! Git isolation for task delegates. Conversation state stays in the parent
//! workspace; file tools run in a separate worktree until an explicit merge.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("running git in {}", root.display()))?;
    ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?)
}
fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(git_output(root, args)?.trim().to_owned())
}

/// The repository a delegate branched from. Recorded at prepare time so a
/// task in any registered project merges back into that project, not into
/// the server's own checkout; older delegates fall back to `root`.
fn repository(root: &Path, spec: &Value) -> PathBuf {
    spec["git"]["repository"].as_str().map_or_else(|| root.to_path_buf(), PathBuf::from)
}

fn worktree_parent(root: &Path) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let parent = root
        .parent()
        .context("repository has no parent directory")?;
    let name = root
        .file_name()
        .context("repository has no directory name")?;
    Ok(parent.join(".codex-workspace-mcp-worktrees").join(name))
}
fn clean_checkout(root: &Path) -> Result<bool> {
    let status = git(root, &["status", "--porcelain", "--untracked-files=normal"])?;
    Ok(status.lines().all(|line| {
        let path = line.get(3..).unwrap_or("");
        path == ".codex-workspace-mcp" || path.starts_with(".codex-workspace-mcp/")
    }))
}

/// The host's per-project index and memory store. It must never be committed
/// by a delegate or overwritten by a merge, so it is excluded repository-wide
/// (the local .git/info/exclude, shared by every worktree; .gitignore is the user's).
const STATE_DIR: &str = ".codex-workspace-mcp";

fn exclude_state_dir(root: &Path) -> Result<()> {
    let common = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    let rule = format!("/{STATE_DIR}/");
    if !current.lines().any(|line| line.trim() == rule) {
        std::fs::create_dir_all(exclude.parent().context("exclude file has no parent")?)?;
        let separator = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&exclude, format!("{current}{separator}{rule}\n"))?;
    }
    Ok(())
}

pub fn prepare(root: &Path, id: &str) -> Result<Option<Value>> {
    let probe = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("Git is required to isolate delegates in a Git project")?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if !root.ancestors().any(|path| path.join(".git").exists()) {
            return Ok(None);
        }
        bail!("could not inspect Git workspace: {}", stderr.trim());
    }
    let top = PathBuf::from(String::from_utf8(probe.stdout)?.trim()).canonicalize()?;
    ensure!(
        top == root.canonicalize()?,
        "GIT_DELEGATE_WORKSPACE_ROOT: select the repository root before delegating Git edits"
    );
    ensure!(
        clean_checkout(root)?,
        "GIT_DELEGATE_DIRTY_BASE: commit or otherwise preserve the current working tree before starting isolated delegates"
    );
    exclude_state_dir(root)?;
    let base = git(root, &["rev-parse", "HEAD"])?;
    let branch = format!("codex/delegate-{id}");
    let path = worktree_parent(root)?.join(id);
    std::fs::create_dir_all(path.parent().context("worktree parent missing")?)?;
    let path_str = path.to_str().context("worktree path is not UTF-8")?;
    git(root, &["worktree", "add", "-b", &branch, path_str, &base])?;
    Ok(Some(
        json!({"branch":branch,"worktree":path,"base":base,"repository":top,"status":"active"}),
    ))
}

pub fn workspace(root: &Path, spec: &Value) -> Result<PathBuf> {
    let root = &repository(root, spec);
    let Some(path) = spec["git"]["worktree"].as_str() else {
        return Ok(root.to_path_buf());
    };
    let path = PathBuf::from(path).canonicalize()?;
    ensure!(
        path.starts_with(worktree_parent(root)?),
        "delegate worktree is outside the managed workspace"
    );
    Ok(path)
}

pub fn finish(spec: &Value) -> Result<Option<Value>> {
    let Some(path) = spec["git"]["worktree"].as_str() else {
        return Ok(None);
    };
    let root = Path::new(path);
    // Also covers worktrees prepared before the exclusion existed.
    exclude_state_dir(root)?;
    git(root, &["add", "-A"])?;
    if !git(root, &["status", "--porcelain"])?.is_empty() {
        git(
            root,
            &[
                "-c",
                "user.name=Codex Delegate",
                "-c",
                "user.email=codex@local",
                "commit",
                "-m",
                "Delegate result",
            ],
        )?;
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    Ok(Some(
        json!({"branch":spec["git"]["branch"],"base":spec["git"]["base"],
        "head":head,"worktree":root,"status":"ready_for_review"}),
    ))
}

pub fn diff(root: &Path, spec: &Value) -> Result<Value> {
    let branch = spec["git"]["branch"]
        .as_str()
        .context("not a Git delegate")?;
    let base = spec["git"]["base"]
        .as_str()
        .context("missing delegate base")?;
    let worktree = workspace(root, spec)?;
    let root = &repository(root, spec);
    ensure!(
        git(&worktree, &["status", "--porcelain"])?.is_empty(),
        "delegate worktree changed after its result was archived; inspect and snapshot it before merging"
    );
    let head = git(root, &["rev-parse", branch])?;
    let stats = git(root, &["diff", "--stat", base, &head])?;
    let patch = git_output(root, &["diff", "--no-ext-diff", base, &head, "--"])?;
    let max = 2_000_000;
    let truncated = patch.len() > max;
    let end = if truncated {
        (0..=max)
            .rev()
            .find(|i| patch.is_char_boundary(*i))
            .unwrap_or(0)
    } else {
        patch.len()
    };
    Ok(json!({"branch":branch,"base":base,"head":head,"stat":stats,
        "diff":&patch[..end],"diff_total_bytes":patch.len(),"diff_truncated":truncated}))
}
pub fn changed_paths(root: &Path, spec: &Value) -> Result<Vec<String>> {
    let branch = spec["git"]["branch"]
        .as_str()
        .context("not a Git delegate")?;
    let base = spec["git"]["base"]
        .as_str()
        .context("missing delegate base")?;
    Ok(
        git_output(&repository(root, spec), &["diff", "--name-only", "-z", base, branch, "--"])?
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

pub fn merge(root: &Path, spec: &Value, expected_head: &str) -> Result<Value> {
    let root = &repository(root, spec);
    let branch = spec["git"]["branch"]
        .as_str()
        .context("not a Git delegate")?;
    let head = git(root, &["rev-parse", branch])?;
    ensure!(
        head == expected_head,
        "delegate branch changed after review; inspect its current diff"
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Codex Delegate",
            "-c",
            "user.email=codex@local",
            "merge",
            "--no-ff",
            "--no-edit",
            branch,
        ])
        .output()?;
    if !output.status.success() {
        bail!(
            "git merge stopped; resolve conflicts in the main worktree and complete or abort the merge: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(
        json!({"branch":branch,"merged_head":head,"integration_head":git(root, &["rev-parse", "HEAD"])?}),
    )
}

pub fn is_merged(root: &Path, spec: &Value, expected_head: &str) -> Result<bool> {
    let root = &repository(root, spec);
    let branch = spec["git"]["branch"]
        .as_str()
        .context("not a Git delegate")?;
    ensure!(
        git(root, &["rev-parse", branch])? == expected_head,
        "delegate branch changed after review"
    );
    Ok(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "--is-ancestor", expected_head, "HEAD"])
        .status()?
        .success())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delegates_merge_into_the_project_they_branched_from() {
        let parent = std::env::temp_dir().join(format!("delegate-repo-{}", crate::agent_service::uuid_like()));
        let (server, project) = (parent.join("server"), parent.join("project"));
        for root in [&server, &project] {
            std::fs::create_dir_all(root).unwrap();
            git(root, &["init", "-b", "main"]).unwrap();
            std::fs::write(root.join("file.txt"), "before\n").unwrap();
            git(root, &["add", "-A"]).unwrap();
            git(root, &["-c", "user.name=Test", "-c", "user.email=test@local", "commit", "-m", "base"]).unwrap();
        }
        let spec = json!({"git":prepare(&project, "child").unwrap().unwrap()});
        // Callers pass the server root; the recorded repository wins.
        let child = workspace(&server, &spec).unwrap();
        assert!(child.starts_with(parent.join(".codex-workspace-mcp-worktrees").join("project")));
        std::fs::write(child.join("file.txt"), "after\n").unwrap();
        // Both checkouts have their own host state store; neither may be committed or clobbered.
        for dir in [&child, &project] {
            std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
            std::fs::write(dir.join(STATE_DIR).join("codex_state.db"), dir.to_string_lossy().as_bytes()).unwrap();
        }
        let head = finish(&spec).unwrap().unwrap()["head"].as_str().unwrap().to_owned();
        assert!(diff(&server, &spec).unwrap()["diff"].as_str().unwrap().contains("+after"));
        assert!(!diff(&server, &spec).unwrap()["stat"].as_str().unwrap().contains(STATE_DIR));
        merge(&server, &spec, &head).unwrap();
        assert!(is_merged(&server, &spec, &head).unwrap());
        assert_eq!(changed_paths(&server, &spec).unwrap(), vec!["file.txt"]);
        assert_eq!(std::fs::read_to_string(project.join("file.txt")).unwrap(), "after\n");
        assert_eq!(std::fs::read_to_string(server.join("file.txt")).unwrap(), "before\n");
        assert_eq!(std::fs::read_to_string(project.join(STATE_DIR).join("codex_state.db")).unwrap(), project.to_string_lossy());
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    fn delegate_branch_is_isolated_until_explicit_merge() {
        let parent = std::env::temp_dir().join(format!(
            "delegate-git-{}",
            crate::agent_service::uuid_like()
        ));
        let root = parent.join("project");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-b", "main"]).unwrap();
        std::fs::write(root.join("file.txt"), "before\n").unwrap();
        git(&root, &["add", "-A"]).unwrap();
        git(
            &root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@local",
                "commit",
                "-m",
                "base",
            ],
        )
        .unwrap();
        let spec = json!({"git":prepare(&root,"child-one").unwrap().unwrap()});
        let child = workspace(&root, &spec).unwrap();
        std::fs::write(child.join("file.txt"), "after\n").unwrap();
        let snapshot = finish(&spec).unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "before\n"
        );
        let diff = diff(&root, &spec).unwrap();
        assert!(diff["diff"].as_str().unwrap().contains("+after"));
        let head = snapshot["head"].as_str().unwrap();
        assert!(!is_merged(&root, &spec, head).unwrap());
        merge(&root, &spec, head).unwrap();
        assert!(is_merged(&root, &spec, head).unwrap());
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "after\n"
        );
        git(
            &root,
            &["worktree", "remove", "--force", child.to_str().unwrap()],
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(parent);
    }
}
