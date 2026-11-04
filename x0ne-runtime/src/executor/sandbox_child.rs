//! Second layer inside bubblewrap: rlimits, then Landlock, then exec.
//!
//! bwrap starts `x0ne-runtime __x0ne-confine <spec> -- cmd args`. Applied after
//! bwrap's mounts (Landlock forbids mounting) and inside its user namespace, so
//! RLIMIT_NPROC counts this sandbox's processes, not the user's.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// `argv[1]` that selects this mode in `x0ne-runtime`'s `main`.
pub const ARG: &str = "__x0ne-confine";

/// Ceilings: TOML may lower them, never raise them (HA-04 reco, 2026-10-05).
pub const MAX_MEMORY_MB: u64 = 4096;
pub const MAX_PIDS: u64 = 512;
pub const MAX_CPU_SECS: u64 = 3600;
pub const MAX_FILE_SIZE_MB: u64 = 4096;

/// Exit status when confinement cannot be applied. Nothing ran.
pub const REFUSED_EXIT: i32 = 126;

/// Read and execute, never write. Missing entries are skipped.
pub const SYSTEM_READ_ROOTS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/etc",
    "/opt",
    "/proc",
    "/sys",
    "/nix/store",
];

/// Toolchains under `$HOME` the agent's builds need, read and execute only.
pub const HOME_READ_ROOTS: &[&str] = &[
    ".cargo",
    ".rustup",
    ".local/bin",
    ".local/lib",
    ".nvm",
    ".pyenv",
    "go",
];

/// Written by the sandboxed process: bwrap's private `/tmp` and minimal `/dev`.
pub const SANDBOX_WRITE_ROOTS: &[&str] = &["/tmp", "/dev"];

/// Per-process limits, as bytes or counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// RLIMIT_DATA, not RLIMIT_AS: V8 and Go reserve far more address space
    /// than they touch, and RLIMIT_AS kills them at start.
    pub data_bytes: u64,
    pub nproc: u64,
    pub cpu_secs: u64,
    pub fsize_bytes: u64,
}

impl Limits {
    pub fn from_config(c: &crate::config::ResourceLimits) -> Self {
        let mb = |v: Option<u64>, max: u64| v.unwrap_or(max).clamp(1, max) * 1024 * 1024;
        Limits {
            data_bytes: mb(c.memory_mb, MAX_MEMORY_MB),
            nproc: c.pids.unwrap_or(MAX_PIDS).clamp(1, MAX_PIDS),
            cpu_secs: c.cpu_secs.unwrap_or(MAX_CPU_SECS).clamp(1, MAX_CPU_SECS),
            fsize_bytes: mb(c.file_size_mb, MAX_FILE_SIZE_MB),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub limits: Limits,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub argv: Vec<OsString>,
}

impl Spec {
    /// Everything after `ARG`.
    pub fn to_args(&self) -> Vec<OsString> {
        let l = &self.limits;
        let mut a: Vec<OsString> = Vec::new();
        for (k, v) in [
            ("--data", l.data_bytes),
            ("--nproc", l.nproc),
            ("--cpu", l.cpu_secs),
            ("--fsize", l.fsize_bytes),
        ] {
            a.push(k.into());
            a.push(v.to_string().into());
        }
        for r in &self.read {
            a.push("--ro".into());
            a.push(r.into());
        }
        for w in &self.write {
            a.push("--rw".into());
            a.push(w.into());
        }
        a.push("--".into());
        a.extend(self.argv.iter().cloned());
        a
    }

    pub fn parse(args: &[OsString]) -> Result<Self, String> {
        let mut it = args.iter();
        let num = |name: &str, v: Option<&OsString>| -> Result<u64, String> {
            v.and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| format!("{name} needs a number"))
        };
        let (mut data, mut nproc, mut cpu, mut fsize) = (None, None, None, None);
        let (mut read, mut write) = (Vec::new(), Vec::new());
        loop {
            let Some(flag) = it.next() else {
                return Err("missing `--` before the command".into());
            };
            match flag.to_str() {
                Some("--data") => data = Some(num("--data", it.next())?),
                Some("--nproc") => nproc = Some(num("--nproc", it.next())?),
                Some("--cpu") => cpu = Some(num("--cpu", it.next())?),
                Some("--fsize") => fsize = Some(num("--fsize", it.next())?),
                Some("--ro") => read.push(PathBuf::from(it.next().ok_or("--ro needs a path")?)),
                Some("--rw") => write.push(PathBuf::from(it.next().ok_or("--rw needs a path")?)),
                Some("--") => break,
                _ => return Err(format!("unknown option {flag:?}")),
            }
        }
        let argv: Vec<OsString> = it.cloned().collect();
        if argv.is_empty() {
            return Err("no command".into());
        }
        let need = |v: Option<u64>, n: &str| v.ok_or(format!("{n} is required"));
        Ok(Spec {
            limits: Limits {
                data_bytes: need(data, "--data")?,
                nproc: need(nproc, "--nproc")?,
                cpu_secs: need(cpu, "--cpu")?,
                fsize_bytes: need(fsize, "--fsize")?,
            },
            read,
            write,
            argv,
        })
    }
}

/// Read roots for one command: system, toolchains, policy read roots. The cwd
/// is not one: `isolation::plan` refuses a cwd outside the roots (H1-1).
pub fn read_roots(home: Option<&Path>, policy_reads: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = SYSTEM_READ_ROOTS.iter().map(PathBuf::from).collect();
    if let Some(h) = home {
        v.extend(HOME_READ_ROOTS.iter().map(|r| h.join(r)));
    }
    v.extend(policy_reads.iter().cloned());
    v.retain(|p| p.exists());
    v.dedup();
    v
}

/// Entry point of `x0ne-runtime __x0ne-confine …`. Never returns.
pub fn main(args: &[OsString]) -> ! {
    let fail = |why: String| -> ! {
        eprintln!("x0ne-confine: command not run: {why}");
        std::process::exit(REFUSED_EXIT)
    };
    let spec = Spec::parse(args).unwrap_or_else(|e| fail(e));
    if let Err(e) = apply_limits(&spec.limits) {
        fail(e)
    }
    if let Err(e) = apply_landlock(&spec.read, &spec.write) {
        fail(e)
    }
    // execvp, not a new command: the environment bwrap passed (already
    // hardened by the runtime, plus the action's own `env`) stays as it is.
    let err = exec(&spec.argv);
    fail(format!(
        "{}: {err}",
        OsStr::new(&spec.argv[0]).to_string_lossy()
    ))
}

fn exec(argv: &[OsString]) -> std::io::Error {
    use std::os::unix::ffi::OsStrExt;
    let c: Result<Vec<std::ffi::CString>, _> = argv
        .iter()
        .map(|a| std::ffi::CString::new(a.as_bytes()))
        .collect();
    let Ok(c) = c else {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in argv");
    };
    let mut ptrs: Vec<*const libc::c_char> = c.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: NUL-terminated strings and array, alive across the call.
    unsafe { libc::execvp(ptrs[0], ptrs.as_ptr()) };
    std::io::Error::last_os_error()
}

fn apply_limits(l: &Limits) -> Result<(), String> {
    for (res, v, name) in [
        (libc::RLIMIT_DATA, l.data_bytes, "RLIMIT_DATA"),
        (libc::RLIMIT_NPROC, l.nproc, "RLIMIT_NPROC"),
        (libc::RLIMIT_CPU, l.cpu_secs, "RLIMIT_CPU"),
        (libc::RLIMIT_FSIZE, l.fsize_bytes, "RLIMIT_FSIZE"),
    ] {
        let lim = libc::rlimit {
            rlim_cur: v as libc::rlim_t,
            rlim_max: v as libc::rlim_t,
        };
        // SAFETY: plain syscall on a stack value.
        if unsafe { libc::setrlimit(res, &lim) } != 0 {
            return Err(format!("{name}: {}", std::io::Error::last_os_error()));
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn apply_landlock(read: &[PathBuf], write: &[PathBuf]) -> Result<(), String> {
    use landlock::{
        path_beneath_rules, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr,
        RulesetStatus, Scope, ABI,
    };
    let abi = ABI::V6;
    let e = |e: landlock::RulesetError| e.to_string();
    let writes: Vec<PathBuf> = SANDBOX_WRITE_ROOTS
        .iter()
        .map(PathBuf::from)
        .chain(write.iter().cloned())
        .collect();
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(e)?
        .scope(Scope::from_all(abi))
        .map_err(e)?
        .create()
        .map_err(e)?
        .add_rules(path_beneath_rules(read, AccessFs::from_read(abi)))
        .map_err(e)?
        .add_rules(path_beneath_rules(&writes, AccessFs::from_all(abi)))
        .map_err(e)?
        .restrict_self()
        .map_err(e)?;
    // No silent fallback: a kernel without Landlock refuses, as a missing bwrap does.
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("Landlock is not available in this kernel".into());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_landlock(_: &[PathBuf], _: &[PathBuf]) -> Result<(), String> {
    Err("Landlock exists only on Linux".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_round_trips() {
        let s = Spec {
            limits: Limits {
                data_bytes: 1,
                nproc: 2,
                cpu_secs: 3,
                fsize_bytes: 4,
            },
            read: vec!["/usr".into()],
            write: vec!["/w s".into()],
            argv: vec!["sh".into(), "-c".into(), "--ro x".into()],
        };
        assert_eq!(Spec::parse(&s.to_args()).unwrap(), s);
    }

    #[test]
    fn a_spec_without_limits_or_command_is_refused() {
        let a = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(Spec::parse(&a(&["--", "true"])).is_err());
        assert!(Spec::parse(&a(&[
            "--data", "1", "--nproc", "1", "--cpu", "1", "--fsize", "1", "--"
        ]))
        .is_err());
        assert!(Spec::parse(&a(&["--data", "x", "--"])).is_err());
    }

    #[test]
    fn toml_can_lower_limits_but_not_raise_them() {
        let c = crate::config::ResourceLimits {
            memory_mb: Some(1 << 40),
            pids: Some(64),
            cpu_secs: None,
            file_size_mb: Some(0),
        };
        let l = Limits::from_config(&c);
        assert_eq!(l.data_bytes, MAX_MEMORY_MB * 1024 * 1024);
        assert_eq!(l.nproc, 64);
        assert_eq!(l.cpu_secs, MAX_CPU_SECS);
        assert_eq!(l.fsize_bytes, 1024 * 1024);
    }
}
