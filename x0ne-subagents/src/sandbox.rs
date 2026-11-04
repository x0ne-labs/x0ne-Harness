//! One sandbox per node: a CoW clone of the parent's work dir (`work`), a
//! frozen copy of the same (`base`, for the diff), and bubblewrap for exec.
//! Only `work` is bound into the process; no net, no host, no env.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, Semaphore};

use crate::node::NodeId;

/// One-way cancellation: set once, observed by the node loop and by exec.
#[derive(Clone, Default)]
pub struct CancelFlag {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelFlag {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            n.await;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecResult {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub elapsed_ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
}

pub struct Sandbox {
    pub id: NodeId,
    pub work: PathBuf,
    pub base: PathBuf,
    pub(crate) bwrap: PathBuf,
}

/// Global cap on simultaneously RUNNING sandboxes (FIFO queue behind it).
pub struct SandboxPool {
    root: PathBuf,
    pub(crate) bwrap: PathBuf,
    permits: Arc<Semaphore>,
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let st = std::process::Command::new("cp")
        .arg("-a")
        .arg("--reflink=auto")
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .status()
        .map_err(|e| format!("cp: {e}"))?;
    if !st.success() {
        return Err(format!("cp exited {st}"));
    }
    Ok(())
}

impl SandboxPool {
    /// Fails closed: without bubblewrap there is no sandbox, hence no node.
    pub fn new(root: &Path, max_concurrent: usize) -> Result<Self, String> {
        let bwrap = ["/usr/bin/bwrap", "/usr/local/bin/bwrap", "/bin/bwrap"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.exists())
            .ok_or("bubblewrap (bwrap) not found: refusing to run nodes without a sandbox")?;
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        Ok(SandboxPool {
            root: root.to_path_buf(),
            bwrap,
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
        })
    }

    pub async fn acquire(&self) -> tokio::sync::OwnedSemaphorePermit {
        Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("semaphore open")
    }

    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Clone `from` (a parent's work dir, or the host workspace for root).
    pub fn clone_from(&self, id: &NodeId, from: &Path) -> Result<Sandbox, String> {
        let dir = self.root.join(id.file_stem());
        let work = dir.join("work");
        let base = dir.join("base");
        copy_tree(from, &work)?;
        copy_tree(from, &base)?;
        Ok(Sandbox {
            id: id.clone(),
            work,
            base,
            bwrap: self.bwrap.clone(),
        })
    }
}

impl Sandbox {
    /// Map a node-supplied path to a real path inside `work`, or refuse.
    /// Absolute paths, `..`, and symlinks leaving the clone are refused.
    pub fn confine(&self, user_path: &str) -> Result<PathBuf, String> {
        let p = Path::new(user_path);
        if p.is_absolute() {
            return Err(format!("absolute path refused: {user_path}"));
        }
        for c in p.components() {
            match c {
                std::path::Component::Normal(_) | std::path::Component::CurDir => {}
                _ => return Err(format!("path leaves the clone: {user_path}")),
            }
        }
        let full = self.work.join(p);
        let root = self.work.canonicalize().map_err(|e| e.to_string())?;
        let check = if full.exists() {
            full.clone()
        } else {
            full.parent().map(Path::to_path_buf).unwrap_or(root.clone())
        };
        if check.exists() {
            let canon = check.canonicalize().map_err(|e| e.to_string())?;
            if !canon.starts_with(&root) {
                return Err(format!("symlink leaves the clone: {user_path}"));
            }
        }
        Ok(full)
    }

    pub fn write(&self, user_path: &str, content: &str) -> Result<PathBuf, String> {
        let p = self.confine(user_path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, content).map_err(|e| e.to_string())?;
        Ok(p)
    }

    pub fn read(&self, user_path: &str) -> Result<String, String> {
        let p = self.confine(user_path)?;
        std::fs::read_to_string(&p).map_err(|e| e.to_string())
    }

    /// `/bin/sh -c cmd` under bwrap: `/usr` read-only, `work` at `/work`,
    /// private tmp, all namespaces unshared (no network), empty env.
    pub async fn exec(&self, cmd: &str, timeout: Duration, cancel: &CancelFlag) -> ExecResult {
        let start = Instant::now();
        let mut c = tokio::process::Command::new(&self.bwrap);
        c.args(["--ro-bind", "/usr", "/usr"])
            .args(["--symlink", "usr/lib", "/lib"])
            .args(["--symlink", "usr/lib64", "/lib64"])
            .args(["--symlink", "usr/bin", "/bin"])
            .args(["--symlink", "usr/sbin", "/sbin"])
            .arg("--bind")
            .arg(&self.work)
            .arg("/work")
            .args(["--tmpfs", "/tmp"])
            .args([
                "--unshare-all",
                "--die-with-parent",
                "--new-session",
                "--clearenv",
            ])
            .args(["--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin"])
            .args(["--setenv", "HOME", "/work"])
            // .pyc files are binary: merge rejects them, keep them out of `work`.
            .args(["--setenv", "PYTHONDONTWRITEBYTECODE", "1"])
            .args(["--chdir", "/work", "--", "/bin/sh", "-c", cmd])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => {
                return ExecResult {
                    exit: 127,
                    stdout: String::new(),
                    stderr: format!("spawn: {e}"),
                    elapsed_ms: 0,
                    timed_out: false,
                    cancelled: false,
                };
            }
        };
        let elapsed = |s: Instant| s.elapsed().as_millis() as u64;
        tokio::select! {
            out = child.wait_with_output() => match out {
                Ok(o) => ExecResult {
                    exit: o.status.code().unwrap_or(-1),
                    stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
                    elapsed_ms: elapsed(start), timed_out: false, cancelled: false,
                },
                Err(e) => ExecResult { exit: -1, stdout: String::new(), stderr: e.to_string(), elapsed_ms: elapsed(start), timed_out: false, cancelled: false },
            },
            _ = tokio::time::sleep(timeout) => ExecResult { exit: -1, stdout: String::new(), stderr: "timeout".into(), elapsed_ms: elapsed(start), timed_out: true, cancelled: false },
            _ = cancel.cancelled() => ExecResult { exit: -1, stdout: String::new(), stderr: "cancelled".into(), elapsed_ms: elapsed(start), timed_out: false, cancelled: true },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_refuses_escapes() {
        let d = tempfile::tempdir().unwrap();
        let sb = Sandbox {
            id: NodeId::root(),
            work: d.path().join("w"),
            base: d.path().join("b"),
            bwrap: PathBuf::from("/bin/true"),
        };
        std::fs::create_dir_all(&sb.work).unwrap();
        assert!(sb.confine("/tmp/x").is_err());
        assert!(sb.confine("../x").is_err());
        assert!(sb.confine("a/../../x").is_err());
        assert!(sb.confine("a/b.txt").unwrap().starts_with(&sb.work));
        std::os::unix::fs::symlink(d.path(), sb.work.join("out")).unwrap();
        assert!(sb.confine("out/x").is_err());
    }
}
