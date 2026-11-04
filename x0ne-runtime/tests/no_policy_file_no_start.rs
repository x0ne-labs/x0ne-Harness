//! Without a policy file the runtime does not start. Every way of reaching
//! "no usable policy" is tried against the real binary: nothing found, an
//! opt-in with nothing behind it, an empty or comment-only file, an
//! unreadable one, an invalid one, a dangling link.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("xdg")).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        Self { _dir: dir, root }
    }
    fn xdg_policy(&self) -> PathBuf {
        self.root.join("xdg").join("x0ne").join("x0ne-runtime.toml")
    }
    fn legacy_policy(&self) -> PathBuf {
        self.root
            .join("xdg")
            .join("x0ne-runtime")
            .join("config.toml")
    }
    fn write(&self, path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_x0ne-runtime"));
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("xdg"))
            .current_dir(self.root.join("work"))
            .args(["--mode", "stdio"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        c
    }
}

/// Run until the policy line or exit. `Ok(stderr)` = it started.
fn start(mut cmd: Command) -> Result<String, String> {
    let mut child = cmd.spawn().expect("spawn x0ne-runtime");
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    let mut line = String::new();
    while err.read_line(&mut line).unwrap_or(0) > 0 {
        seen.push_str(&line);
        if line.starts_with("x0ne-runtime: mode=") {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(seen);
        }
        line.clear();
    }
    let status = child.wait().unwrap();
    assert!(!status.success(), "exited 0 without starting:\n{seen}");
    Err(seen)
}

fn refused(cmd: Command, what: &str) -> String {
    match start(cmd) {
        Ok(out) => panic!("{what}: the runtime started anyway:\n{out}"),
        Err(out) => {
            assert!(
                out.contains("refusing to start"),
                "{what}: no clear refusal:\n{out}"
            );
            out
        }
    }
}

const VALID: &str = "[policy]\nmode = \"auto\"\ndefault_effect = \"deny\"\n";

#[test]
fn no_policy_anywhere_is_refused_and_says_what_to_write_where() {
    let h = Home::new();
    let out = refused(h.cmd(&[]), "no file");
    let want = h.xdg_policy();
    assert!(
        out.contains(&want.display().to_string()),
        "the message must name the file to write, {}:\n{out}",
        want.display()
    );
    // 2026-10-16: it named `x0ne setup`, the sandbox bootstrap, which
    // writes nothing here without incus or podman. The wizard is `x0ne --setup`.
    assert!(
        out.contains("`x0ne --setup`"),
        "and how to write it:\n{out}"
    );
    assert!(
        !out.contains("`x0ne setup`"),
        "not the sandbox bootstrap:\n{out}"
    );
    assert!(out.contains("--config"), "and the alternative:\n{out}");
}

#[test]
fn the_cwd_opt_in_with_no_file_behind_it_is_refused() {
    let h = Home::new();
    let mut c = h.cmd(&[]);
    c.env("X0NE_RUNTIME_ALLOW_CWD_CONFIG", "1");
    refused(c, "opt-in, no file");
}

#[test]
fn a_cwd_file_without_the_opt_in_does_not_count_as_a_policy() {
    let h = Home::new();
    h.write(&h.root.join("work").join("x0ne-runtime.toml"), VALID);
    refused(h.cmd(&[]), "cwd file, no opt-in");
}

#[test]
fn an_empty_or_comment_only_file_is_refused() {
    for text in ["", "   \n\n", "# nothing here\n  # still nothing\n"] {
        let h = Home::new();
        h.write(&h.xdg_policy(), text);
        let out = refused(h.cmd(&[]), &format!("xdg file {text:?}"));
        assert!(out.contains("`x0ne --setup`"), "{out}");
        let cfg = h.root.join("explicit.toml");
        h.write(&cfg, text);
        refused(
            h.cmd(&["--config", cfg.to_str().unwrap()]),
            &format!("--config file {text:?}"),
        );
    }
}

#[test]
fn an_unreadable_file_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let h = Home::new();
    h.write(&h.xdg_policy(), VALID);
    std::fs::set_permissions(h.xdg_policy(), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(h.xdg_policy()).is_ok() {
        return; // root reads anything; the case cannot be built here
    }
    refused(h.cmd(&[]), "unreadable");
}

#[test]
fn an_invalid_policy_is_refused() {
    for text in [
        "[policy\n",
        "[policy]\ndefault_effect = \"maybe\"\n",
        "[actions]\nNoSuchAction = true\n",
    ] {
        let h = Home::new();
        h.write(&h.xdg_policy(), text);
        refused(h.cmd(&[]), &format!("invalid {text:?}"));
    }
}

#[test]
fn a_dangling_link_does_not_fall_through_to_another_file() {
    let h = Home::new();
    std::fs::create_dir_all(h.xdg_policy().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(h.root.join("gone.toml"), h.xdg_policy()).unwrap();
    h.write(
        &h.legacy_policy(),
        "[policy]\ndefault_effect = \"allow\"\nallowed_write_paths = [\"/\"]\n",
    );
    refused(h.cmd(&[]), "dangling link before a legacy file");
}

#[test]
fn a_missing_explicit_file_is_refused() {
    let h = Home::new();
    let missing = h.root.join("missing.toml");
    refused(
        h.cmd(&["--config", missing.to_str().unwrap()]),
        "--config missing",
    );
}

#[test]
fn a_real_policy_still_starts() {
    let h = Home::new();
    h.write(&h.xdg_policy(), VALID);
    let out = start(h.cmd(&[])).unwrap_or_else(|e| panic!("refused a valid policy:\n{e}"));
    assert!(out.contains(&h.xdg_policy().display().to_string()), "{out}");
    let cfg = h.root.join("explicit.toml");
    h.write(&cfg, VALID);
    start(h.cmd(&["--config", cfg.to_str().unwrap()]))
        .unwrap_or_else(|e| panic!("refused --config:\n{e}"));
}
