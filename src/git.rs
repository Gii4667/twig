use anyhow::{Context, Result};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

use crate::config::{GlobalConfig, Project};

/// Message types for streaming command output
#[derive(Debug, Clone)]
pub enum CommandOutput {
    /// A line of output (stdout or stderr combined)
    Line(String),
    /// Command started with its description
    CommandStarted(String),
    /// Command completed with success/failure
    CommandFinished { success: bool, command: String },
    /// All commands completed
    AllDone { success: bool },
}

/// Runs post-create commands in a background thread with streaming output
pub struct CommandRunner {
    receiver: Receiver<CommandOutput>,
    handle: Option<thread::JoinHandle<()>>,
}

impl CommandRunner {
    /// Start running commands in a background thread
    pub fn new(commands: Vec<String>, work_dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();

        let handle = thread::spawn(move || {
            let mut all_success = true;

            for cmd_str in &commands {
                // Notify command start
                let _ = tx.send(CommandOutput::CommandStarted(cmd_str.clone()));

                // Spawn the command with piped output
                let mut child = match Command::new("sh")
                    .arg("-c")
                    .arg(cmd_str)
                    .current_dir(&work_dir)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(e) => {
                        let _ = tx.send(CommandOutput::Line(format!("Failed to start: {}", e)));
                        let _ = tx.send(CommandOutput::CommandFinished {
                            success: false,
                            command: cmd_str.clone(),
                        });
                        all_success = false;
                        continue;
                    }
                };

                // Read stdout in a thread
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let tx_stdout = tx.clone();
                let tx_stderr = tx.clone();

                let stdout_handle = stdout.map(|out| {
                    thread::spawn(move || {
                        let reader = BufReader::new(out);
                        for line in reader.lines().map_while(Result::ok) {
                            let _ = tx_stdout.send(CommandOutput::Line(line));
                        }
                    })
                });

                let stderr_handle = stderr.map(|err| {
                    thread::spawn(move || {
                        let reader = BufReader::new(err);
                        for line in reader.lines().map_while(Result::ok) {
                            let _ = tx_stderr.send(CommandOutput::Line(line));
                        }
                    })
                });

                // Wait for output threads
                if let Some(h) = stdout_handle {
                    let _ = h.join();
                }
                if let Some(h) = stderr_handle {
                    let _ = h.join();
                }

                // Wait for command to finish
                let success = match child.wait() {
                    Ok(status) => status.success(),
                    Err(_) => false,
                };

                let _ = tx.send(CommandOutput::CommandFinished {
                    success,
                    command: cmd_str.clone(),
                });

                if !success {
                    all_success = false;
                }
            }

            let _ = tx.send(CommandOutput::AllDone {
                success: all_success,
            });
        });

        CommandRunner {
            receiver: rx,
            handle: Some(handle),
        }
    }

    /// Try to receive output without blocking
    pub fn try_recv(&self) -> Option<CommandOutput> {
        self.receiver.try_recv().ok()
    }

    /// Check if the runner is still active
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }
}

/// Create a git worktree for a project
pub fn create_worktree(project: &Project, branch: &str) -> Result<PathBuf> {
    let config = GlobalConfig::load()?;
    let project_root = project.root_expanded();

    // Worktree path: {worktree_base}/{project}/{branch}
    let branch_safe = branch.replace('/', "-");
    let worktree_path = config
        .worktree_base_expanded()
        .join(&project.name)
        .join(&branch_safe);

    // Check if worktree already exists
    if worktree_path.exists() {
        anyhow::bail!("Worktree already exists at {:?}", worktree_path);
    }

    // Ensure parent directory exists
    if let Some(parent) = worktree_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {:?}", parent))?;
    }

    // Check if branch exists locally or remotely
    let branch_exists = check_branch_exists(&project_root, branch)?;

    // Create the worktree (suppress output to avoid breaking TUI)
    let mut cmd = Command::new("git");
    cmd.current_dir(&project_root);
    cmd.arg("worktree").arg("add");

    if branch_exists {
        // Checkout existing branch
        cmd.arg(&worktree_path).arg(branch);
    } else {
        // Create new branch from current HEAD
        cmd.arg("-b").arg(branch).arg(&worktree_path);
    }

    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to create git worktree")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr.trim());
    }

    // Copy files if configured
    if let Some(wt_config) = &project.worktree {
        for file in &wt_config.copy {
            let src = project_root.join(file);
            let dst = worktree_path.join(file);

            if src.exists() {
                // Create parent directories if needed
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).ok();
                }

                copy_path_preserve_symlinks(&src, &dst)?;
            }
        }

        for file in &wt_config.symlink {
            let src = project_root.join(file);
            let dst = worktree_path.join(file);

            if src.exists() {
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent).ok();
                }

                create_symlink(&src, &dst)?;
            }
        }
    }

    Ok(worktree_path)
}

/// Get post-create commands for a project (if any)
pub fn get_post_create_commands(project: &Project) -> Vec<String> {
    project
        .worktree
        .as_ref()
        .map(|w| w.post_create.clone())
        .unwrap_or_default()
}

/// Start streaming post-create commands execution
pub fn start_post_create_commands(
    project: &Project,
    worktree_path: &Path,
) -> Option<CommandRunner> {
    let commands = get_post_create_commands(project);
    if commands.is_empty() {
        return None;
    }
    Some(CommandRunner::new(commands, worktree_path.to_path_buf()))
}

/// Run post-create commands in the worktree directory (blocking, for non-TUI use)
pub fn run_post_create_commands(project: &Project, worktree_path: &Path) -> Result<()> {
    if let Some(wt_config) = &project.worktree {
        for cmd_str in &wt_config.post_create {
            println!("Running: {}", cmd_str);

            let status = Command::new("sh")
                .arg("-c")
                .arg(cmd_str)
                .current_dir(worktree_path)
                .status()
                .with_context(|| format!("Failed to run: {}", cmd_str))?;

            if !status.success() {
                eprintln!("Warning: command failed: {}", cmd_str);
            }
        }
    }

    Ok(())
}

/// Delete a git worktree and its local branch
pub fn delete_worktree(project: &Project, branch: &str) -> Result<()> {
    let config = GlobalConfig::load()?;
    let project_root = project.root_expanded();

    let branch_safe = branch.replace('/', "-");
    let worktree_path = config
        .worktree_base_expanded()
        .join(&project.name)
        .join(&branch_safe);

    if !worktree_path.exists() {
        anyhow::bail!("Worktree does not exist at {:?}", worktree_path);
    }

    // Remove the worktree (suppress output to avoid breaking TUI)
    let output = Command::new("git")
        .current_dir(&project_root)
        .args(["worktree", "remove", "--force"])
        .arg(&worktree_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to remove git worktree")?;

    if !output.status.success() {
        // Try force removal of the directory
        fs::remove_dir_all(&worktree_path)
            .with_context(|| format!("Failed to remove worktree directory: {:?}", worktree_path))?;

        // Prune worktree references
        Command::new("git")
            .current_dir(&project_root)
            .args(["worktree", "prune"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }

    // Delete the local branch
    delete_local_branch(&project_root, branch)?;

    Ok(())
}

/// Delete a local git branch
fn delete_local_branch(repo_path: &Path, branch: &str) -> Result<()> {
    // Force delete the branch (-D) since the worktree is already removed
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["branch", "-D", branch])
        .output()
        .context("Failed to delete local branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Ignore error if branch doesn't exist (may have been a remote-tracking branch)
        if !stderr.contains("not found") {
            eprintln!(
                "Warning: could not delete branch '{}': {}",
                branch,
                stderr.trim()
            );
        }
    }

    Ok(())
}

/// List worktrees for a project
pub fn list_worktrees(project: &Project) -> Result<Vec<WorktreeInfo>> {
    let config = GlobalConfig::load()?;
    let project_root = project.root_expanded();

    let output = Command::new("git")
        .current_dir(&project_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .context("Failed to list git worktrees")?;

    if !output.status.success() {
        return Ok(vec![]);
    }

    let stdout = String::from_utf8(output.stdout)?;
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    let worktree_base = config.worktree_base_expanded().join(&project.name);

    for line in stdout.lines() {
        if line.starts_with("worktree ") {
            // Save previous worktree if any
            if let (Some(path), Some(branch)) = (current_path.take(), current_branch.take()) {
                // Only include worktrees under our worktree_base
                if path.starts_with(&worktree_base) {
                    worktrees.push(WorktreeInfo { path, branch });
                }
            }

            current_path = Some(PathBuf::from(line.strip_prefix("worktree ").unwrap()));
        } else if line.starts_with("branch ") {
            let branch = line
                .strip_prefix("branch refs/heads/")
                .unwrap_or(line.strip_prefix("branch ").unwrap_or(""));
            current_branch = Some(branch.to_string());
        }
    }

    // Don't forget the last one
    if let (Some(path), Some(branch)) = (current_path, current_branch) {
        if path.starts_with(&worktree_base) {
            worktrees.push(WorktreeInfo { path, branch });
        }
    }

    Ok(worktrees)
}

#[derive(Debug)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,
}

/// Check if a branch exists (locally or remotely)
fn check_branch_exists(repo_path: &Path, branch: &str) -> Result<bool> {
    // Check local branches
    let local = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", branch])
        .output()?;

    if local.status.success() {
        return Ok(true);
    }

    // Check remote branches
    let remote = Command::new("git")
        .current_dir(repo_path)
        .args(["rev-parse", "--verify", &format!("origin/{}", branch)])
        .output()?;

    Ok(remote.status.success())
}

/// Get the default branch (main or master) for a repository
pub fn get_default_branch(repo_path: &Path) -> Result<String> {
    // Try to get from remote HEAD
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD", "--short"])
        .output()
        .context("Failed to get default branch")?;

    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout)
            .trim()
            .strip_prefix("origin/")
            .unwrap_or("main")
            .to_string();
        return Ok(branch);
    }

    // Fallback: check if main or master exists
    for branch in ["main", "master"] {
        let status = Command::new("git")
            .current_dir(repo_path)
            .args(["rev-parse", "--verify", branch])
            .output()?;

        if status.status.success() {
            return Ok(branch.to_string());
        }
    }

    Ok("main".to_string())
}

/// Merge a branch into the default branch (main/master)
pub fn merge_branch_to_default(repo_path: &Path, branch: &str) -> Result<()> {
    let default_branch = get_default_branch(repo_path)?;

    // Checkout default branch (suppress output to avoid breaking TUI)
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["checkout", &default_branch])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to checkout default branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to checkout '{}': {}", default_branch, stderr.trim());
    }

    // Merge the branch (suppress output to avoid breaking TUI)
    let output = Command::new("git")
        .current_dir(repo_path)
        .args(["merge", branch])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to merge branch")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Merge failed: {}. Please resolve conflicts manually in the main repository.",
            stderr.trim()
        );
    }

    Ok(())
}

/// Copy a file or directory, preserving symlinks
fn copy_path_preserve_symlinks(src: &Path, dst: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(src)
        .with_context(|| format!("Failed to read metadata for {:?}", src))?;

    if metadata.file_type().is_symlink() {
        let target = fs::read_link(src)
            .with_context(|| format!("Failed to read symlink target for {:?}", src))?;
        create_symlink(&target, dst)?;
        return Ok(());
    }

    if metadata.is_dir() {
        copy_dir_recursive(src, dst)?;
    } else {
        fs::copy(src, dst).with_context(|| format!("Failed to copy {:?} to {:?}", src, dst))?;
    }

    Ok(())
}

/// Recursively copy a directory, preserving symlinks
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        copy_path_preserve_symlinks(&src_path, &dst_path)?;
    }

    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;

    symlink(target, link)
        .with_context(|| format!("Failed to create symlink {:?} -> {:?}", link, target))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> Result<()> {
    anyhow::bail!("Symlink copying is only supported on Unix systems")
}
