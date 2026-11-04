//! The podman executable, from a fixed path only.
//!
//! `child_env::harden` keeps PATH, so a bare `podman` runs whatever comes
//! first in it, including a directory the model can write. Same rule as
//! `BWRAP_LOCATIONS` in `isolation.rs`: root-managed locations, PATH never.

/// Where podman is looked for, in order. PATH is not searched.
pub const PODMAN_LOCATIONS: &[&str] = &["/usr/bin/podman", "/usr/local/bin/podman", "/bin/podman"];

/// The first candidate that is a regular file with an execute bit.
pub fn first_executable<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    use std::os::unix::fs::PermissionsExt;
    candidates.iter().copied().find(|p| {
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// The podman binary, or `None` when none of [`PODMAN_LOCATIONS`] holds one.
pub fn podman_path() -> Option<&'static str> {
    first_executable(PODMAN_LOCATIONS)
}

/// For error messages: where podman was looked for.
pub fn not_found_message() -> String {
    format!(
        "podman not found at {} (PATH is not searched) — install Podman: https://podman.io/docs/installation",
        PODMAN_LOCATIONS.join(", ")
    )
}

/// A blocking podman command, by absolute path.
pub fn std_command() -> Option<std::process::Command> {
    podman_path().map(std::process::Command::new)
}

/// An async podman command, by absolute path.
pub fn tokio_command() -> Option<tokio::process::Command> {
    podman_path().map(tokio::process::Command::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn only_an_executable_regular_file_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        let subdir = dir.path().join("subdir");
        let exe = dir.path().join("exe");
        std::fs::write(&plain, "x").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(&exe, "x").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let missing = dir.path().join("missing");
        let c = [
            missing.to_str().unwrap(),
            plain.to_str().unwrap(),
            subdir.to_str().unwrap(),
            exe.to_str().unwrap(),
        ];
        assert_eq!(first_executable(&c), Some(c[3]));
        assert_eq!(first_executable(&c[..3]), None);
    }

    #[test]
    fn every_location_is_absolute_and_root_managed() {
        for p in PODMAN_LOCATIONS {
            assert!(
                p.starts_with("/usr/bin/")
                    || p.starts_with("/usr/local/bin/")
                    || p.starts_with("/bin/"),
                "{p}"
            );
        }
        let msg = not_found_message();
        assert!(
            msg.contains("/usr/bin/podman") && msg.contains("PATH is not searched"),
            "{msg}"
        );
    }

    #[test]
    fn a_command_names_the_absolute_path() {
        match (podman_path(), std_command()) {
            (Some(p), Some(c)) => assert_eq!(c.get_program(), p),
            (None, None) => {}
            other => panic!("podman_path and std_command disagree: {other:?}"),
        }
    }
}
