//! Where a parallel world works, what it cost, and how its work comes back.
//!
//! Each world gets its own directory under `<base>/.x0ne-worlds/<id>`: a
//! detached `git worktree` when `<base>` is the top of a repository with a
//! commit, otherwise a copy (reflink when the filesystem allows) with a
//! baseline commit of its own. Either way the world dir is a git repository,
//! so `diff`, `pick` and `drop` have one implementation.
//!
//! The ledger records only what a world task reported: its dir, its budget,
//! its status and its measured cost. Nothing here invents a row.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};

/// The directory, under the base, that holds every world dir.
pub const WORLDS_DIR: &str = ".x0ne-worlds";

/// How a world dir was made, which decides how it is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    Worktree,
    Copy,
}

/// A world's own directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldDir {
    pub base: PathBuf,
    pub path: PathBuf,
    pub kind: DirKind,
}

fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir);
    // A world's commits are bookkeeping, not authorship.
    c.args([
        "-c",
        "user.name=x0ne-worlds",
        "-c",
        "user.email=worlds@x0ne.invalid",
        "-c",
        "commit.gpgsign=false",
    ]);
    c
}

fn run(mut c: Command, what: &str) -> Result<String> {
    let out = c
        .output()
        .with_context(|| format!("{what}: git not runnable"))?;
    if !out.status.success() {
        bail!(
            "{what}: {}",
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .lines()
                .last()
                .unwrap_or("failed")
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether `base` is the top of a git repository that has a commit.
fn is_repo_top(base: &Path) -> bool {
    let top = git(base).args(["rev-parse", "--show-toplevel"]).output();
    let head = git(base)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output();
    match (top, head) {
        (Ok(t), Ok(h)) if t.status.success() && h.status.success() => {
            let t = PathBuf::from(String::from_utf8_lossy(&t.stdout).trim());
            t.canonicalize().ok() == base.canonicalize().ok()
        }
        _ => false,
    }
}

/// Keep `.x0ne-worlds/` out of the base repository's status.
fn exclude_worlds_dir(base: &Path) {
    if let Ok(p) = run(
        {
            let mut c = git(base);
            c.args(["rev-parse", "--git-path", "info/exclude"]);
            c
        },
        "git-path",
    ) {
        let p = base.join(p.trim());
        let line = format!("/{WORLDS_DIR}/");
        let cur = std::fs::read_to_string(&p).unwrap_or_default();
        if !cur.lines().any(|l| l.trim() == line) {
            if let Some(d) = p.parent() {
                let _ = std::fs::create_dir_all(d);
            }
            let sep = if cur.is_empty() || cur.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let _ = std::fs::write(&p, format!("{cur}{sep}{line}\n"));
        }
    }
}

/// Entries of `base` a copy leaves out: the worlds themselves and the
/// sandbox's home, which holds sessions and keys, not project files.
fn skipped(name: &str) -> bool {
    name == WORLDS_DIR || name == ".x0ne-home"
}

/// Create the world dir for `id` under `base`.
pub fn prepare(base: &Path, id: &str) -> Result<WorldDir> {
    if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
        bail!("world id {id:?} cannot name a directory");
    }
    let base = base
        .canonicalize()
        .with_context(|| format!("worlds base {} does not exist", base.display()))?;
    let root = base.join(WORLDS_DIR);
    let path = root.join(id);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    std::fs::create_dir_all(&root)?;

    if is_repo_top(&base) {
        exclude_worlds_dir(&base);
        let mut c = git(&base);
        c.args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg("HEAD");
        run(c, "git worktree add")?;
        return Ok(WorldDir {
            base,
            path,
            kind: DirKind::Worktree,
        });
    }

    std::fs::create_dir_all(&path)?;
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let name = entry.file_name();
        if skipped(&name.to_string_lossy()) {
            continue;
        }
        let st = Command::new("cp")
            .args(["-a", "--reflink=auto"])
            .arg(entry.path())
            .arg(&path)
            .status()
            .context("cp not runnable")?;
        if !st.success() {
            let _ = std::fs::remove_dir_all(&path);
            bail!("copy of {} failed", entry.path().display());
        }
    }
    let baseline = (|| -> Result<()> {
        run(
            {
                let mut c = git(&path);
                c.args(["init", "-q"]);
                c
            },
            "git init",
        )?;
        run(
            {
                let mut c = git(&path);
                c.args(["add", "-A"]);
                c
            },
            "git add",
        )?;
        run(
            {
                let mut c = git(&path);
                c.args(["commit", "-q", "--allow-empty", "-m", "baseline"]);
                c
            },
            "git commit",
        )?;
        Ok(())
    })();
    if let Err(e) = baseline {
        let _ = std::fs::remove_dir_all(&path);
        return Err(e);
    }
    Ok(WorldDir {
        base,
        path,
        kind: DirKind::Copy,
    })
}

/// What the world changed against its starting point, new files included.
pub fn diff(dir: &WorldDir) -> Result<String> {
    run(
        {
            let mut c = git(&dir.path);
            // Bytecode a test run leaves behind is not the world's work.
            c.args([
                "add",
                "-A",
                "--",
                ".",
                ":(exclude,glob)**/__pycache__/**",
                ":(exclude,glob)**/*.pyc",
            ]);
            c
        },
        "git add",
    )?;
    run(
        {
            let mut c = git(&dir.path);
            c.args(["diff", "--cached", "--binary", "HEAD"]);
            c
        },
        "git diff",
    )
}

/// One line per changed file, with added and removed line counts.
pub fn diffstat(dir: &WorldDir) -> Result<String> {
    diff(dir)?;
    run(
        {
            let mut c = git(&dir.path);
            c.args(["diff", "--cached", "--stat", "HEAD"]);
            c
        },
        "git diff --stat",
    )
}

/// Apply the world's diff to its base. Checked first: a patch that does not
/// apply cleanly changes nothing.
pub fn pick(dir: &WorldDir) -> Result<usize> {
    let patch = diff(dir)?;
    if patch.trim().is_empty() {
        bail!("{} changed nothing", dir.path.display());
    }
    let file = dir.base.join(WORLDS_DIR).join(format!(
        ".pick-{}.patch",
        dir.path.file_name().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&file, &patch)?;
    let apply = |check: bool| -> Result<String> {
        let mut c = git(&dir.base);
        // In a copy, the base may sit inside some outer repository; stop git
        // from finding it, so the patch lands relative to the base.
        if dir.kind == DirKind::Copy {
            if let Some(parent) = dir.base.parent() {
                c.env("GIT_CEILING_DIRECTORIES", parent);
            }
        }
        c.arg("apply");
        if check {
            c.arg("--check");
        }
        c.arg(&file);
        run(c, "git apply")
    };
    let res = apply(true).and_then(|_| apply(false));
    let _ = std::fs::remove_file(&file);
    res?;
    Ok(patch.matches("diff --git ").count())
}

/// Remove the world dir.
pub fn drop_dir(dir: &WorldDir) -> Result<()> {
    match dir.kind {
        DirKind::Worktree => {
            let mut c = git(&dir.base);
            c.args(["worktree", "remove", "--force"]).arg(&dir.path);
            run(c, "git worktree remove")?;
        }
        DirKind::Copy => {
            if dir.path.exists() {
                std::fs::remove_dir_all(&dir.path)?;
            }
        }
    }
    // Leave no empty `.x0ne-worlds/` behind.
    let _ = std::fs::remove_dir(dir.base.join(WORLDS_DIR));
    Ok(())
}

// ── Budget ─────────────────────────────────────────────────────────────────

/// Dollars committed to worlds, against the session's budget.
///
/// A world reserves its slice before it starts; when it ends, the slice is
/// replaced by what it actually spent. The sum of reservations and spend can
/// never pass the cap, so N worlds together cannot outspend `/budget`.
#[derive(Debug, Default)]
pub struct WorldsBudget {
    committed: Mutex<f64>,
}

impl WorldsBudget {
    /// Reserve a slice. `cap` is the session's limit (0 = none), `parent_spent`
    /// what the parent already spent, `want` the slice asked for, `share` how
    /// many ways to split what is left when nothing was asked.
    /// Returns the slice in USD; `0.0` means no dollar cap applies.
    pub fn reserve(
        &self,
        cap: f64,
        parent_spent: f64,
        want: Option<f64>,
        share: usize,
    ) -> Result<f64, String> {
        let mut committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        if cap <= 0.0 {
            return Ok(want.filter(|w| *w > 0.0).unwrap_or(0.0));
        }
        let avail = cap - parent_spent - *committed;
        if avail < 0.0005 {
            return Err(format!(
                "worlds budget exhausted: ${:.4} committed to worlds, ${:.4} spent by the session, cap ${:.2}",
                *committed, parent_spent, cap
            ));
        }
        let slice = want
            .filter(|w| *w > 0.0)
            .unwrap_or(avail / share.max(1) as f64)
            .min(avail);
        *committed += slice;
        Ok(slice)
    }

    /// Replace a reservation by the measured spend.
    pub fn settle(&self, reserved: f64, spent: f64) {
        let mut committed = self.committed.lock().unwrap_or_else(|e| e.into_inner());
        *committed = (*committed - reserved + spent).max(0.0);
    }

    pub fn committed(&self) -> f64 {
        *self.committed.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ── Ledger ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Running,
    Succeeded,
    Failed(String),
}

/// One world, as its task reported it.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub task: String,
    pub dir: Option<WorldDir>,
    pub budget_usd: f64,
    pub token_cap: u64,
    pub status: Status,
    pub cost_usd: Option<f64>,
    pub tokens: u64,
    pub actions: usize,
    pub duration_ms: u64,
}

#[derive(Default)]
pub struct Ledger {
    rows: Mutex<BTreeMap<String, Entry>>,
}

impl Ledger {
    pub fn start(&self, entry: Entry) {
        self.lock().insert(entry.id.clone(), entry);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish(
        &self,
        id: &str,
        status: Status,
        cost_usd: Option<f64>,
        tokens: u64,
        actions: usize,
        duration_ms: u64,
    ) {
        if let Some(e) = self.lock().get_mut(id) {
            e.status = status;
            e.cost_usd = cost_usd;
            e.tokens = tokens;
            e.actions = actions;
            e.duration_ms = duration_ms;
        }
    }

    pub fn get(&self, id: &str) -> Option<Entry> {
        self.lock().get(id).cloned()
    }

    pub fn all(&self) -> Vec<Entry> {
        let mut v: Vec<Entry> = self.lock().values().cloned().collect();
        v.sort_by_key(|e| world_number(&e.id).unwrap_or(usize::MAX));
        v
    }

    pub fn remove(&self, id: &str) -> Option<Entry> {
        self.lock().remove(id)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.rows.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn ledger() -> &'static Ledger {
    static L: std::sync::OnceLock<Ledger> = std::sync::OnceLock::new();
    L.get_or_init(Ledger::default)
}

fn world_number(id: &str) -> Option<usize> {
    id.strip_prefix("world-")?.parse().ok()
}

/// `2` or `world-2` → `world-2`.
pub fn resolve_id(arg: &str) -> String {
    let a = arg.trim();
    if a.chars().all(|c| c.is_ascii_digit()) && !a.is_empty() {
        format!("world-{a}")
    } else {
        a.to_string()
    }
}

/// The `/worlds` table: one row per world the session started.
pub fn render_table(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return "no worlds. Start some with /spawn <N> <prompt>\n".into();
    }
    let mut out = format!(
        "{:<10} {:<9} {:>9} {:>9} {:>8} {:>7} {:>7}  {}\n",
        "world", "status", "cost", "budget", "tokens", "actions", "time", "dir"
    );
    let mut total = 0.0;
    let mut unpriced = 0;
    for e in entries {
        let status = match &e.status {
            Status::Running => "running",
            Status::Succeeded => "ok",
            Status::Failed(_) => "failed",
        };
        let cost = match (&e.status, e.cost_usd) {
            (Status::Running, _) => "…".to_string(),
            (_, Some(c)) => {
                total += c;
                format!("${c:.4}")
            }
            (_, None) => {
                unpriced += 1;
                "unknown".to_string()
            }
        };
        let budget = if e.budget_usd > 0.0 {
            format!("${:.4}", e.budget_usd)
        } else {
            "none".into()
        };
        let time = if e.status == Status::Running {
            "…".to_string()
        } else {
            format!("{:.1}s", e.duration_ms as f64 / 1000.0)
        };
        let dir = e
            .dir
            .as_ref()
            .map(|d| d.path.display().to_string())
            .unwrap_or_else(|| "(shared, no isolation)".into());
        out.push_str(&format!(
            "{:<10} {:<9} {:>9} {:>9} {:>8} {:>7} {:>7}  {}\n",
            e.id, status, cost, budget, e.tokens, e.actions, time, dir
        ));
        if let Status::Failed(why) = &e.status {
            let why: String = why.chars().take(160).collect();
            out.push_str(&format!("           └ {why}\n"));
        }
    }
    out.push_str(&format!("total measured cost: ${total:.4}"));
    if unpriced > 0 {
        out.push_str(&format!(
            " ({unpriced} world(s) without a price, not counted)"
        ));
    }
    out.push('\n');
    out
}

/// Apply world `id` to its base. Refused while it runs.
pub fn pick_world(id: &str) -> Result<(WorldDir, usize)> {
    let e = ledger().get(id).ok_or_else(|| anyhow!("no world {id}"))?;
    if e.status == Status::Running {
        bail!("{id} is still running");
    }
    let dir = e
        .dir
        .ok_or_else(|| anyhow!("{id} ran without its own dir: nothing to pick"))?;
    let n = pick(&dir)?;
    Ok((dir, n))
}

/// Remove every finished world's dir and row. Running worlds are kept.
/// Returns the ids dropped and the ids kept.
pub fn drop_finished() -> (Vec<String>, Vec<String>, Vec<String>) {
    let (mut dropped, mut kept, mut errors) = (Vec::new(), Vec::new(), Vec::new());
    for e in ledger().all() {
        if e.status == Status::Running {
            kept.push(e.id);
            continue;
        }
        if let Some(dir) = &e.dir {
            if let Err(err) = drop_dir(dir) {
                errors.push(format!("{}: {err}", e.id));
                continue;
            }
        }
        ledger().remove(&e.id);
        dropped.push(e.id);
    }
    (dropped, kept, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let st = git(dir).args(args).status().unwrap();
        assert!(st.success(), "git {args:?}");
    }

    #[test]
    fn a_repo_base_gets_worktrees_and_pick_applies_the_chosen_diff() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        sh(&base, &["init", "-q"]);
        std::fs::write(base.join("a.txt"), "one\n").unwrap();
        sh(&base, &["add", "-A"]);
        sh(&base, &["commit", "-q", "-m", "init"]);

        let w1 = prepare(&base, "world-1").unwrap();
        let w2 = prepare(&base, "world-2").unwrap();
        assert_eq!(w1.kind, DirKind::Worktree);
        std::fs::write(w1.path.join("a.txt"), "from world 1\n").unwrap();
        std::fs::write(w2.path.join("a.txt"), "from world 2\n").unwrap();
        std::fs::write(w2.path.join("new.py"), "x = 2\n").unwrap();
        std::fs::create_dir_all(w2.path.join("__pycache__")).unwrap();
        std::fs::write(w2.path.join("__pycache__/new.cpython-313.pyc"), "bytecode").unwrap();

        let d2 = diff(&w2).unwrap();
        assert!(d2.contains("+x = 2"));
        assert!(!d2.contains("__pycache__"), "{d2}");
        assert_eq!(pick(&w2).unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(base.join("a.txt")).unwrap(),
            "from world 2\n"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("new.py")).unwrap(),
            "x = 2\n"
        );
        // The worlds dir stays out of the base's status.
        let st = git(&base).args(["status", "--porcelain"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&st.stdout).contains(WORLDS_DIR));

        drop_dir(&w1).unwrap();
        drop_dir(&w2).unwrap();
        assert!(!w1.path.exists() && !w2.path.exists());
        assert!(!base.join(WORLDS_DIR).exists());
        let wt = git(&base).args(["worktree", "list"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);
    }

    #[test]
    fn a_plain_dir_gets_copies_and_pick_lands_relative_to_the_base() {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().join("proj");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/m.py"), "a = 1\n").unwrap();
        std::fs::create_dir_all(base.join(".x0ne-home")).unwrap();

        let w = prepare(&base, "world-3").unwrap();
        assert_eq!(w.kind, DirKind::Copy);
        assert!(!w.path.join(".x0ne-home").exists());
        std::fs::write(w.path.join("src/m.py"), "a = 3\n").unwrap();
        pick(&w).unwrap();
        assert_eq!(
            std::fs::read_to_string(base.join("src/m.py")).unwrap(),
            "a = 3\n"
        );
        drop_dir(&w).unwrap();
        assert!(!base.join(WORLDS_DIR).exists());
    }

    #[test]
    fn the_budget_pool_never_lets_worlds_pass_the_cap() {
        let pool = WorldsBudget::default();
        // $0.30 split three ways, the parent having spent nothing.
        let a = pool.reserve(0.30, 0.0, Some(0.10), 3).unwrap();
        let b = pool.reserve(0.30, 0.0, Some(0.10), 3).unwrap();
        let c = pool.reserve(0.30, 0.0, Some(0.50), 3).unwrap();
        assert!((a + b + c - 0.30).abs() < 1e-9, "{a} {b} {c}");
        assert!(pool.reserve(0.30, 0.0, None, 1).is_err());
        // A world that spent less gives the rest back.
        pool.settle(a, 0.02);
        assert!((pool.reserve(0.30, 0.0, None, 1).unwrap() - 0.08).abs() < 1e-9);
        // No cap: no dollar limit unless asked.
        assert_eq!(WorldsBudget::default().reserve(0.0, 0.0, None, 3), Ok(0.0));
    }

    #[test]
    fn the_table_shows_only_what_was_reported() {
        assert!(render_table(&[]).starts_with("no worlds"));
        let row = Entry {
            id: "world-1".into(),
            task: "t".into(),
            dir: None,
            budget_usd: 0.1,
            token_cap: 0,
            status: Status::Running,
            cost_usd: None,
            tokens: 0,
            actions: 0,
            duration_ms: 0,
        };
        let t = render_table(std::slice::from_ref(&row));
        assert!(t.contains("running") && t.contains('…'), "{t}");
        assert!(!t.contains('✓'), "{t}");
    }
}
