//! On-disk persistence for goals.
//!
//! Layout:
//!   ~/.x0ne/goals/
//!     active            # text file, single line: id of the active goal (or empty)
//!     <goal_id>.json    # one file per goal
//!
//! Writes are atomic (tempfile + rename) so a crash mid-write never produces
//! a half-written goal file. Reads are best-effort: a corrupted file is
//! reported as `Err` and the caller decides whether to clear or recover.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use super::state::GoalState;

const ACTIVE_POINTER: &str = "active";

pub struct GoalStore {
    root: PathBuf,
}

impl GoalStore {
    /// Default location under the user's home: `~/.x0ne/goals/`. The
    /// directory is created on first use.
    pub fn default_for_home() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
        Self::with_root(home.join(".x0ne").join("goals"))
    }

    pub fn with_root<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create goal store at {:?}", root))?;
        Ok(Self { root })
    }

    fn goal_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.json"))
    }

    fn active_pointer(&self) -> PathBuf {
        self.root.join(ACTIVE_POINTER)
    }

    /// Atomic write: serialize → tempfile in same dir → fsync → rename.
    pub fn save(&self, goal: &GoalState) -> Result<()> {
        let final_path = self.goal_path(&goal.id);
        let json = serde_json::to_string_pretty(goal)
            .with_context(|| format!("failed to serialize goal {}", goal.id))?;
        atomic_write(&final_path, json.as_bytes())
    }

    pub fn load(&self, id: &str) -> Result<GoalState> {
        let path = self.goal_path(id);
        let bytes =
            fs::read(&path).with_context(|| format!("failed to read goal file {:?}", path))?;
        let state: GoalState = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse goal {} (corrupt file?)", id))?;
        Ok(state)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.goal_path(id).is_file()
    }

    /// Remove the goal file from disk. No-op if missing. Also clears the
    /// active pointer if it matched.
    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.goal_path(id);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to delete goal file {:?}", path))?;
        }
        if self.read_active_id()?.as_deref() == Some(id) {
            self.clear_active()?;
        }
        Ok(())
    }

    /// Read the active pointer. `Ok(None)` if the pointer doesn't exist or
    /// is empty. The pointer is intentionally a tiny text file so that a
    /// human can `cat` / `echo > active` to debug.
    pub fn read_active_id(&self) -> Result<Option<String>> {
        let p = self.active_pointer();
        match fs::read_to_string(&p) {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_active(&self, id: &str) -> Result<()> {
        atomic_write(&self.active_pointer(), id.as_bytes())
    }

    pub fn clear_active(&self) -> Result<()> {
        let p = self.active_pointer();
        match fs::remove_file(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Convenience: load the active goal if one is set and the file exists.
    /// Returns `Ok(None)` when there is no active goal or the pointer is
    /// dangling (corrupt manifest, removed file, …) — in the dangling case
    /// the pointer is auto-cleared.
    pub fn get_active(&self) -> Result<Option<GoalState>> {
        let Some(id) = self.read_active_id()? else {
            return Ok(None);
        };
        if !self.exists(&id) {
            // Self-heal a dangling pointer instead of erroring forever.
            let _ = self.clear_active();
            return Ok(None);
        }
        self.load(&id).map(Some)
    }
}

fn atomic_write(final_path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = final_path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {:?}", final_path))?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".x0ne-goal-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create tempfile in {:?}", parent))?;
    tmp.write_all(bytes)?;
    tmp.flush()?;
    tmp.as_file().sync_all().ok();
    tmp.persist(final_path)
        .map_err(|e| anyhow!("atomic rename failed: {}", e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::state::{Budget, GoalState, GoalStatus};

    fn make_store() -> (tempfile::TempDir, GoalStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = GoalStore::with_root(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn save_load_round_trip() {
        let (_dir, store) = make_store();
        let g = GoalState::new(
            "goal_1".into(),
            "first".into(),
            Budget::new(Some(10), None),
            None,
        );
        store.save(&g).unwrap();
        let back = store.load("goal_1").unwrap();
        assert_eq!(back.id, "goal_1");
        assert_eq!(back.text, "first");
        assert_eq!(back.status, GoalStatus::Active);
        assert_eq!(back.budget.max_steps, Some(10));
    }

    #[test]
    fn active_pointer_round_trip() {
        let (_dir, store) = make_store();
        assert!(store.read_active_id().unwrap().is_none());
        store.set_active("goal_42").unwrap();
        assert_eq!(store.read_active_id().unwrap().as_deref(), Some("goal_42"));
        store.clear_active().unwrap();
        assert!(store.read_active_id().unwrap().is_none());
    }

    #[test]
    fn get_active_self_heals_dangling_pointer() {
        let (_dir, store) = make_store();
        store.set_active("goal_missing").unwrap();
        let g = store.get_active().unwrap();
        assert!(g.is_none());
        // Pointer should have been cleared.
        assert!(store.read_active_id().unwrap().is_none());
    }

    #[test]
    fn delete_clears_active_when_matching() {
        let (_dir, store) = make_store();
        let g = GoalState::new(
            "goal_x".into(),
            "to delete".into(),
            Budget::new(None, None),
            None,
        );
        store.save(&g).unwrap();
        store.set_active("goal_x").unwrap();
        store.delete("goal_x").unwrap();
        assert!(!store.exists("goal_x"));
        assert!(store.read_active_id().unwrap().is_none());
    }
}
