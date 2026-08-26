//! Turns a free-form task description into a concrete workspace for an agent:
//! which branch to check out in a worktree, whether to create a new branch
//! (and from which base), or whether to work directly on the project's base
//! checkout. The decision is delegated to an LLM (the `claude` CLI in print
//! mode); if that fails we fall back to a new branch derived from the task.

use anyhow::{anyhow, bail, Context as _, Result};
use serde::Deserialize;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::state::{data_dir, WorkspaceMode};

#[derive(Deserialize, Debug, Clone)]
pub struct TaskPlan {
    /// "existing_branch" | "new_branch" | "base"
    pub mode: String,
    /// Branch to check out (existing_branch) or to create (new_branch).
    #[serde(default)]
    pub branch: Option<String>,
    /// Base branch for new_branch mode. Defaults to the repo's default branch.
    #[serde(default)]
    pub base_branch: Option<String>,
    /// Short (2-4 word) display name for the agent.
    #[serde(default)]
    pub agent_name: Option<String>,
}

/// The resolved workspace for an agent.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub workdir: PathBuf,
    pub branch: Option<String>,
    pub agent_name: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?} in {}", repo.display()))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn is_git_repo(path: &Path) -> bool {
    git(path, &["rev-parse", "--git-dir"]).is_ok()
}

fn local_branches(repo: &Path) -> Vec<String> {
    git(repo, &["branch", "--format=%(refname:short)"])
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

fn remote_branches(repo: &Path) -> Vec<String> {
    git(repo, &["branch", "-r", "--format=%(refname:short)"])
        .map(|s| {
            s.lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.contains("HEAD"))
                .collect()
        })
        .unwrap_or_default()
}

pub fn default_branch(repo: &Path) -> String {
    if let Ok(head) = git(repo, &["symbolic-ref", "refs/remotes/origin/HEAD"]) {
        if let Some(name) = head.trim().strip_prefix("refs/remotes/origin/") {
            return name.to_string();
        }
    }
    let locals = local_branches(repo);
    for candidate in ["main", "master"] {
        if locals.iter().any(|b| b == candidate) {
            return candidate.to_string();
        }
    }
    git(repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "main".to_string())
}

fn slugify(text: &str, max_words: usize) -> String {
    let slug: Vec<String> = text
        .split_whitespace()
        .take(max_words)
        .map(|w| {
            w.chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug.join("-")
    }
}

/// Open PRs as JSON via the `gh` CLI, best effort (None if gh is missing,
/// there is no GitHub remote, or the call fails).
fn open_prs(repo: &Path) -> Option<String> {
    let out = Command::new("gh")
        .args(["pr", "list", "--json", "number,title,headRefName", "--limit", "50"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty() && s != "[]").then_some(s)
}

/// Model used by the default planner command. Planning is a tiny
/// classification job, so a small model is the sane default: a `claude -p`
/// call carries a fixed session overhead that a full-priced model would spend
/// ~50x more on.
pub const DEFAULT_MODEL: &str = "haiku";

/// Planner configuration from the persisted settings, as edited in the
/// settings dialog. The `HARMONIUM_PLANNER_*` variables still override these
/// for one-off runs.
#[derive(Clone, Debug, Default)]
pub struct PlannerSettings {
    /// Full command line; when set, `preset_command` and `model` are unused.
    pub command: String,
    /// Argv derived from the selected agent preset, already split, already
    /// `$VAR`-expanded, and already carrying the print-mode flags and the
    /// model. Empty when no preset is selected.
    pub preset_argv: Vec<String>,
    /// Environment from that preset, so planning runs with the same settings
    /// as the work — a sandbox preset plans inside the sandbox.
    pub env: Vec<(String, String)>,
    /// Model for the default `claude -p --model <model>` command.
    pub model: String,
}

/// Ask the LLM to derive a plan from the task description, using the `claude`
/// CLI in non-interactive print mode. The prompt is appended to the resolved
/// command as its final argument.
///
/// The command is resolved in this order, highest first:
///
/// 1. `HARMONIUM_PLANNER_CMD` — the whole command line.
/// 2. `HARMONIUM_PLANNER_MODEL` — just the `--model` argument. Setting both
///    variables is an error.
/// 3. `settings.command`, the planner command from the settings dialog.
/// 4. `settings.model`, falling back to [`DEFAULT_MODEL`].
pub fn plan_task(repo: &Path, task: &str, settings: &PlannerSettings) -> Result<TaskPlan> {
    let locals = local_branches(repo);
    let remotes = remote_branches(repo);
    let default = default_branch(repo);
    let prs = open_prs(repo);

    let prompt = format!(
        r##"You are a planning assistant for an agent orchestrator. A coding agent will be \
spawned to work on a task inside a git repository. Decide how its workspace should be set up.

Repository default branch: {default}
Local branches:
{locals}
Remote branches:
{remotes}
Open pull requests (JSON: number, title, headRefName):
{prs}

Task description:
{task}

Reply with ONLY a JSON object (no prose, no code fences) of this shape:
{{
  "mode": "existing_branch" | "new_branch" | "base",
  "branch": string | null,
  "base_branch": string | null,
  "agent_name": string
}}

Rules:
- "existing_branch": ONLY if the task refers to work on an existing branch or PR AND that \
branch literally appears in the lists above (match remote branches without the remote \
prefix; for a PR, use its headRefName). Set "branch" to it exactly as listed. Never \
invent a branch name for this mode; if no listed branch matches, use "new_branch".
- "new_branch": normal case for a fresh task. Set "branch" to a new kebab-case branch name \
derived from the task, and "base_branch" to the branch to fork from ({default} unless the \
task says otherwise).
- "base": only if the task explicitly asks to work directly on the current checkout \
without a worktree.
- "agent_name": a 2-4 word human-readable summary of the task. EXCEPTION: if the task is \
about an existing pull request, it MUST have the form "#<PR number> <2-4 word summary>" \
(e.g. "#42 login crash fix"); take the number from the PR list above or from the task text."##,
        locals = if locals.is_empty() { "(none)".into() } else { locals.join("\n") },
        remotes = if remotes.is_empty() { "(none)".into() } else { remotes.join("\n") },
        prs = prs.unwrap_or_else(|| "(none or unavailable)".into()),
    );

    // An exported-but-empty override counts as unset: otherwise an empty
    // _MODEL turns into a bare `--model` that swallows the prompt, and an
    // empty _CMD would trip the mutual-exclusion check against a real _MODEL.
    let override_var = |name: &str| -> Option<String> {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    };
    let cmd_override = override_var("HARMONIUM_PLANNER_CMD");
    let model_override = override_var("HARMONIUM_PLANNER_MODEL");
    if cmd_override.is_some() && model_override.is_some() {
        bail!(
            "only one of HARMONIUM_PLANNER_CMD and HARMONIUM_PLANNER_MODEL may be set"
        );
    }
    let non_empty = |value: &str| {
        let value = value.trim().to_string();
        (!value.is_empty()).then_some(value)
    };
    // Env first (one-off overrides), then the settings, then the default. A
    // configured command wins over a configured model, which is what the
    // settings dialog says it does.
    // The selected agent preset arrives already split and expanded; the other
    // sources are command *lines* and are split here.
    let (command, mut parts) = match (cmd_override, model_override) {
        (Some(command), _) => (command.clone(), split_command(&command)),
        (None, Some(model)) => {
            let command = planner_command_for(&model)?;
            (command.clone(), split_command(&command))
        }
        (None, None) => match non_empty(&settings.command) {
            Some(command) => (command.clone(), split_command(&command)),
            None if !settings.preset_argv.is_empty() => {
                (settings.preset_argv.join(" "), settings.preset_argv.clone())
            }
            None => {
                let command = planner_command_for(
                    &non_empty(&settings.model).unwrap_or_else(|| DEFAULT_MODEL.to_string()),
                )?;
                (command.clone(), split_command(&command))
            }
        },
    };
    if parts.is_empty() {
        bail!("empty planner command");
    }
    crate::log::info(format!("planner: running `{command}` for task: {task}"));
    let program = parts.remove(0);
    let out = Command::new(&program)
        .args(&parts)
        .arg(&prompt)
        // In the repository, so a sandboxed preset wraps the project rather
        // than whatever directory harmonium happens to have been started in.
        .current_dir(repo)
        .envs(settings.env.iter().cloned())
        .env_remove("ANTHROPIC_LOG")
        .output()
        .with_context(|| format!("running planner `{command}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let message = if stderr.trim().is_empty() { stdout } else { stderr };
        bail!("planner failed: {}", snippet(message.trim()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    crate::log::info(format!("planner replied: {}", snippet(text.trim())));
    let plan = parse_plan_json(&text)?;
    crate::log::info(format!("planner plan: {plan:?}"));
    Ok(plan)
}

/// The default planner command for a model name. A multi-word name would
/// inject extra CLI flags, so it is rejected.
fn planner_command_for(model: &str) -> Result<String> {
    if model.chars().any(char::is_whitespace) {
        bail!("planner model must be a single word, got `{model}`");
    }
    Ok(format!("claude -p --model {model}"))
}

/// First ~200 chars of a message, for error reporting.
fn snippet(text: &str) -> String {
    let text = text.trim();
    if text.is_empty() {
        return "(no output)".into();
    }
    let mut end = text.len().min(200);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        format!("{}…", &text[..end])
    } else {
        text.to_string()
    }
}

fn parse_plan_json(text: &str) -> Result<TaskPlan> {
    // Be forgiving about surrounding prose or code fences: extract the first
    // top-level JSON object. Include the planner's actual reply in errors so
    // messages like "session limit reached" surface to the user.
    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("planner did not reply with JSON: {}", snippet(text)))?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    for (i, c) in text[start..].char_indices() {
        if escape {
            escape = false;
            continue;
        }
        match c {
            '\\' if in_str => escape = true,
            '"' => in_str = !in_str,
            '{' if !in_str => depth += 1,
            '}' if !in_str => {
                depth -= 1;
                if depth == 0 {
                    let candidate = &text[start..start + i + c.len_utf8()];
                    return serde_json::from_str(candidate)
                        .with_context(|| format!("parsing planner JSON: {candidate}"));
                }
            }
            _ => {}
        }
    }
    bail!("unterminated JSON in planner output: {}", snippet(text))
}

/// Override the workspace the planner chose with the one the user asked for.
/// Only the *placement* is forced — the agent name stays whatever the planner
/// derived from the task, which is why the planner runs in every mode.
///
/// Forcing a worktree keeps the planner's branch when it named one (including
/// an existing branch the task refers to, which `resolve_workspace` then
/// checks out in a worktree of its own), and derives one otherwise: the LLM
/// leaves `branch` empty whenever it meant to work on the base checkout.
pub fn apply_workspace_mode(plan: &mut TaskPlan, mode: WorkspaceMode, task: &str) {
    match mode {
        WorkspaceMode::Auto => {}
        WorkspaceMode::NewWorktree => {
            if plan.mode == "base" {
                plan.mode = "new_branch".into();
            }
            if plan.branch.as_deref().unwrap_or("").trim().is_empty() {
                let source = plan
                    .agent_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(task);
                plan.branch = Some(format!("harmonium/{}", slugify(source, 4)));
            }
        }
        WorkspaceMode::MainBranch => {
            plan.mode = "base".into();
            plan.branch = None;
            plan.base_branch = None;
        }
    }
}

/// Fallback plan when the LLM is unavailable: new branch off the default branch.
pub fn fallback_plan(repo: &Path, task: &str) -> TaskPlan {
    TaskPlan {
        mode: "new_branch".into(),
        branch: Some(format!("harmonium/{}", slugify(task, 4))),
        base_branch: Some(default_branch(repo)),
        agent_name: Some(slugify(task, 3).replace('-', " ")),
    }
}

/// Split a command line into program + args, honoring quotes and backslash
/// escapes (a small subset of shell word splitting).
pub fn split_command(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars();
    let mut in_single = false;
    let mut in_double = false;
    let mut had_quotes = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                had_quotes = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                had_quotes = true;
            }
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() || had_quotes {
                    parts.push(std::mem::take(&mut current));
                }
                had_quotes = false;
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() || had_quotes {
        parts.push(current);
    }
    parts
}

fn sanitize_for_path(branch: &str) -> String {
    branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

/// Directory under which harmonium places worktrees for `repo`.
fn worktrees_dir(repo: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    repo.hash(&mut hasher);
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    data_dir()
        .join("worktrees")
        .join(format!("{name}-{:08x}", hasher.finish() as u32))
}

/// Find an existing worktree that has `branch` checked out. Git enforces one
/// worktree per branch, so if one exists we must reuse it.
fn existing_worktree_for_branch(repo: &Path, branch: &str) -> Option<PathBuf> {
    let out = git(repo, &["worktree", "list", "--porcelain"]).ok()?;
    let mut current_path: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            let short = b.strip_prefix("refs/heads/").unwrap_or(b);
            if short == branch {
                return current_path.clone();
            }
        }
    }
    None
}

/// Whether `workdir` has uncommitted work: staged, modified or untracked
/// files. Ignored files don't count — build output shouldn't pin a worktree
/// down. This is the same question `git worktree remove` asks before it
/// refuses, asked early so the refusal can be explained.
pub fn is_dirty(workdir: &Path) -> Result<bool> {
    Ok(!git(workdir, &["status", "--porcelain"])?.trim().is_empty())
}

/// Delete the worktree at `workdir`, both its directory and the metadata
/// `repo` keeps for it. The **branch is left alone**: its commits are what the
/// work actually is, and they stay reachable once the checkout is gone.
pub fn remove_worktree(repo: &Path, workdir: &Path) -> Result<()> {
    git(
        repo,
        &["worktree", "remove", &workdir.to_string_lossy()],
    )?;
    Ok(())
}

fn branch_exists_locally(repo: &Path, branch: &str) -> bool {
    git(repo, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]).is_ok()
}

fn remote_ref_for_branch(repo: &Path, branch: &str) -> Option<String> {
    let remotes = remote_branches(repo);
    remotes
        .into_iter()
        .find(|r| r.split_once('/').map(|(_, b)| b == branch).unwrap_or(false))
}

/// Resolve a plan into an actual directory, creating a worktree if necessary.
pub fn resolve_workspace(repo: &Path, plan: &TaskPlan, task: &str) -> Result<Workspace> {
    let agent_name = plan
        .agent_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| slugify(task, 3).replace('-', " "));

    match plan.mode.as_str() {
        "base" => Ok(Workspace {
            workdir: repo.to_path_buf(),
            branch: None,
            agent_name,
        }),
        "existing_branch" | "new_branch" => {
            let branch = plan
                .branch
                .clone()
                .filter(|b| !b.trim().is_empty())
                .ok_or_else(|| anyhow!("plan mode {} requires a branch", plan.mode))?;

            // One worktree per branch: reuse if it already exists.
            if let Some(path) = existing_worktree_for_branch(repo, &branch) {
                return Ok(Workspace {
                    workdir: path,
                    branch: Some(branch),
                    agent_name,
                });
            }

            let dir = worktrees_dir(repo).join(sanitize_for_path(&branch));
            std::fs::create_dir_all(dir.parent().unwrap())?;
            let dir_str = dir.to_string_lossy().into_owned();

            if plan.mode == "existing_branch" {
                if branch_exists_locally(repo, &branch) {
                    git(repo, &["worktree", "add", &dir_str, &branch])?;
                } else if let Some(remote_ref) = remote_ref_for_branch(repo, &branch) {
                    // Create a local tracking branch in the new worktree.
                    git(
                        repo,
                        &["worktree", "add", "-b", &branch, &dir_str, &remote_ref],
                    )?;
                } else {
                    bail!("branch `{branch}` not found locally or on any remote");
                }
            } else {
                let base = plan
                    .base_branch
                    .clone()
                    .filter(|b| !b.trim().is_empty())
                    .unwrap_or_else(|| default_branch(repo));
                if branch_exists_locally(repo, &branch) {
                    // Planner said "new" but it already exists; just use it.
                    git(repo, &["worktree", "add", &dir_str, &branch])?;
                } else {
                    git(repo, &["worktree", "add", "-b", &branch, &dir_str, &base])?;
                }
            }
            Ok(Workspace {
                workdir: dir,
                branch: Some(branch),
                agent_name,
            })
        }
        other => bail!("unknown plan mode `{other}`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let plan = parse_plan_json(
            r#"{"mode":"new_branch","branch":"fix-login","base_branch":"main","agent_name":"fix login"}"#,
        )
        .unwrap();
        assert_eq!(plan.mode, "new_branch");
        assert_eq!(plan.branch.as_deref(), Some("fix-login"));
    }

    #[test]
    fn parses_fenced_json_with_prose() {
        let plan = parse_plan_json(
            "Sure! Here is the plan:\n```json\n{\"mode\": \"base\", \"branch\": null, \"agent_name\": \"quick fix\"}\n```\n",
        )
        .unwrap();
        assert_eq!(plan.mode, "base");
        assert!(plan.branch.is_none());
    }

    #[test]
    fn worktree_lifecycle() {
        let base = std::env::temp_dir().join(format!("harmonium-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        // Put the worktrees this test makes next to everything else it makes,
        // so removing `base` at the end takes them with it. Isolation from a
        // running harmonium doesn't depend on this line — `data_dir()` refuses
        // to resolve to a real session under `cfg(test)` — but the cleanup
        // does. Tests in this module run in one process; only this one touches
        // the env var.
        std::env::set_var("HARMONIUM_DATA_DIR", base.join("data"));

        let run = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "hello").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        run(&["branch", "feature/existing"]);

        // New branch off main.
        let plan = TaskPlan {
            mode: "new_branch".into(),
            branch: Some("harmonium/test-task".into()),
            base_branch: Some("main".into()),
            agent_name: Some("test task".into()),
        };
        let ws = resolve_workspace(&repo, &plan, "test task").unwrap();
        assert!(ws.workdir.join("a.txt").exists());
        assert_eq!(ws.branch.as_deref(), Some("harmonium/test-task"));

        // Same branch again: must reuse the same worktree (one per branch).
        let ws2 = resolve_workspace(&repo, &plan, "test task").unwrap();
        assert_eq!(ws.workdir, ws2.workdir);

        // Existing branch.
        let plan = TaskPlan {
            mode: "existing_branch".into(),
            branch: Some("feature/existing".into()),
            base_branch: None,
            agent_name: None,
        };
        let ws3 = resolve_workspace(&repo, &plan, "continue feature").unwrap();
        assert!(ws3.workdir.join("a.txt").exists());
        assert_ne!(ws3.workdir, ws.workdir);

        // Base mode: work directly in the repo.
        let plan = TaskPlan {
            mode: "base".into(),
            branch: None,
            base_branch: None,
            agent_name: Some("quick".into()),
        };
        let ws4 = resolve_workspace(&repo, &plan, "quick").unwrap();
        assert_eq!(ws4.workdir, repo);
        assert!(ws4.branch.is_none());

        // A fresh worktree is clean; any uncommitted work makes it dirty.
        assert!(!is_dirty(&ws.workdir).unwrap());
        std::fs::write(ws.workdir.join("b.txt"), "wip").unwrap();
        assert!(is_dirty(&ws.workdir).unwrap(), "untracked file counts");
        // Removal refuses while it is dirty, so no work can be lost.
        assert!(remove_worktree(&repo, &ws.workdir).is_err());
        std::fs::remove_file(ws.workdir.join("b.txt")).unwrap();
        assert!(!is_dirty(&ws.workdir).unwrap());

        // Clean: the checkout goes, the branch stays.
        remove_worktree(&repo, &ws.workdir).unwrap();
        assert!(!ws.workdir.exists());
        assert!(branch_exists_locally(&repo, "harmonium/test-task"));
        assert!(existing_worktree_for_branch(&repo, "harmonium/test-task").is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn split_command_words() {
        assert_eq!(split_command("claude"), vec!["claude"]);
        assert_eq!(
            split_command("claude-isol --local -- --continue"),
            vec!["claude-isol", "--local", "--", "--continue"]
        );
        assert_eq!(
            split_command(r#"claude-isol -v "/my data:/data" --local"#),
            vec!["claude-isol", "-v", "/my data:/data", "--local"]
        );
        assert_eq!(split_command("  "), Vec::<String>::new());
    }

    fn plan(mode: &str, branch: Option<&str>) -> TaskPlan {
        TaskPlan {
            mode: mode.into(),
            branch: branch.map(str::to_string),
            base_branch: Some("main".into()),
            agent_name: Some("fix login crash".into()),
        }
    }

    #[test]
    fn workspace_mode_auto_changes_nothing() {
        let mut p = plan("base", None);
        apply_workspace_mode(&mut p, WorkspaceMode::Auto, "work on the checkout");
        assert_eq!(p.mode, "base");
        assert_eq!(p.branch, None);
    }

    #[test]
    fn workspace_mode_forces_a_worktree() {
        // The planner wanted the base checkout and named no branch, so one is
        // derived from the name it *did* give.
        let mut p = plan("base", None);
        apply_workspace_mode(&mut p, WorkspaceMode::NewWorktree, "just look around");
        assert_eq!(p.mode, "new_branch");
        assert_eq!(p.branch.as_deref(), Some("harmonium/fix-login-crash"));

        // A branch the planner chose is kept, existing ones included.
        let mut p = plan("existing_branch", Some("feature/login"));
        apply_workspace_mode(&mut p, WorkspaceMode::NewWorktree, "continue the login work");
        assert_eq!(p.mode, "existing_branch");
        assert_eq!(p.branch.as_deref(), Some("feature/login"));

        // No agent name to derive from: fall back to the task text.
        let mut p = TaskPlan {
            mode: "base".into(),
            branch: Some("  ".into()),
            base_branch: None,
            agent_name: None,
        };
        apply_workspace_mode(&mut p, WorkspaceMode::NewWorktree, "Add a Retry Button");
        assert_eq!(p.branch.as_deref(), Some("harmonium/add-a-retry-button"));
    }

    #[test]
    fn workspace_mode_forces_the_base_checkout() {
        let mut p = plan("new_branch", Some("harmonium/whatever"));
        apply_workspace_mode(&mut p, WorkspaceMode::MainBranch, "fix the login crash");
        assert_eq!(p.mode, "base");
        assert_eq!(p.branch, None);
        assert_eq!(p.base_branch, None);
        // The name is the planner's in every mode.
        assert_eq!(p.agent_name.as_deref(), Some("fix login crash"));
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Fix the Login-Bug now!", 3), "fix-the-loginbug");
        assert_eq!(slugify("", 3), "task");
    }
}
