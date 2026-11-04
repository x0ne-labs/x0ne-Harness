//! Sandbox mode: declared by `scripts/run-sandboxed.sh` (`X0NE_SANDBOX`),
//! confirmed by a container marker, never inferred from the bridge. Inside a
//! sandbox the default is yolo: no confirmation prompts, but the runtime
//! policy still refuses what it forbids and nothing lifts it but a human.
//! `X0NE_SANDBOX_CONFIRM=1` brings the prompts back.

use std::sync::OnceLock;

/// Files whose presence says "this process runs in a container".
pub const CONTAINER_MARKERS: &[&str] = &[
    "/run/.containerenv",
    "/.dockerenv",
    "/dev/incus",
    "/run/systemd/container",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {
    /// Not in a declared sandbox: the human gate applies.
    Host,
    /// Declared and confirmed; every per-action and plan prompt is lifted.
    Yolo { runtime: String },
    /// Declared and confirmed, but the operator asked for prompts.
    Confirm { runtime: String },
    /// Declared but not confirmed (no container marker, unknown runtime):
    /// treated as the host, with this reason shown once.
    Unconfirmed { reason: String },
}

pub fn detect_with(
    get: impl Fn(&str) -> Option<String>,
    exists: impl Fn(&str) -> bool,
) -> SandboxMode {
    let get = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let Some(runtime) = get("X0NE_SANDBOX") else {
        return SandboxMode::Host;
    };
    if !matches!(runtime.as_str(), "incus" | "podman") {
        return SandboxMode::Unconfirmed {
            reason: format!("X0NE_SANDBOX={runtime} is not a known sandbox (incus, podman)"),
        };
    }
    if !CONTAINER_MARKERS.iter().any(|m| exists(m)) {
        return SandboxMode::Unconfirmed {
            reason: format!(
                "X0NE_SANDBOX={runtime} is set, but this process is not in a container"
            ),
        };
    }
    let confirm = get("X0NE_SANDBOX_CONFIRM").is_some_and(|v| {
        matches!(
            v.to_ascii_lowercase().as_str(),
            "1" | "y" | "yes" | "true" | "on"
        )
    });
    if confirm {
        SandboxMode::Confirm { runtime }
    } else {
        SandboxMode::Yolo { runtime }
    }
}

impl SandboxMode {
    pub fn yolo(&self) -> bool {
        matches!(self, SandboxMode::Yolo { .. })
    }
    pub fn in_sandbox(&self) -> bool {
        matches!(self, SandboxMode::Yolo { .. } | SandboxMode::Confirm { .. })
    }
    /// The banner line, e.g. "yolo · sandbox (incus)"; `None` on the host.
    pub fn banner(&self) -> Option<String> {
        match self {
            SandboxMode::Host => None,
            SandboxMode::Yolo { runtime } => Some(format!("yolo · sandbox ({runtime})")),
            SandboxMode::Confirm { runtime } => Some(format!("confirm · sandbox ({runtime})")),
            SandboxMode::Unconfirmed { reason } => Some(format!("confirm · host ({reason})")),
        }
    }
}

/// Set by run-sandboxed.sh when `X0NE_SANDBOX_NET=open` lifted the egress
/// proxy: the banner must say the sandbox can reach any host.
pub fn egress_warning_with(
    mode: &SandboxMode,
    get: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let open = get("X0NE_SANDBOX_NET").is_some_and(|v| v.trim().eq_ignore_ascii_case("open"));
    (mode.in_sandbox() && open).then(|| "network OPEN (X0NE_SANDBOX_NET=open)".to_string())
}

/// [`egress_warning_with`] for this process.
pub fn egress_warning() -> Option<String> {
    egress_warning_with(current(), |k| std::env::var(k).ok())
}

/// This process's mode, read once.
pub fn current() -> &'static SandboxMode {
    static MODE: OnceLock<SandboxMode> = OnceLock::new();
    MODE.get_or_init(|| {
        detect_with(
            |k| std::env::var(k).ok(),
            |p| std::path::Path::new(p).exists(),
        )
    })
}

/// `X0NE_REQUIRE_SANDBOX`: on unless unset, empty or "0". Any other
/// value counts: `true` used to leave the session on the host.
pub fn sandbox_required_with(get: impl Fn(&str) -> Option<String>) -> bool {
    get("X0NE_REQUIRE_SANDBOX").is_some_and(|v| !matches!(v.trim(), "" | "0"))
}

/// [`sandbox_required_with`] for this process.
pub fn sandbox_required() -> bool {
    sandbox_required_with(|k| std::env::var(k).ok())
}

/// Why the actions of a runtime reached through this bridge would not provably
/// land in a container; `None` when they would. `process`: where x0ne runs.
pub fn uncontained_reason(
    mode: &str,
    sandbox_cmd: Option<&str>,
    process: &SandboxMode,
) -> Option<String> {
    use crate::container_runtime::{Backend, Containment};
    match mode {
        "sandbox" => {
            // The bridge executes the first token; the detector skips KEY=VALUE ones.
            let executed = sandbox_cmd
                .and_then(|c| c.split_whitespace().next())
                .and_then(Backend::parse);
            let contained =
                executed.is_some() && Containment::detect(mode, sandbox_cmd).is_contained();
            (!contained).then(|| {
                "bridge.sandbox_cmd is not `incus|podman|docker exec <container> …`, \
                 so nothing proves it runs in a container"
                    .to_string()
            })
        }
        "http" => Some(
            "bridge.mode \"http\" reaches x0ne-runtime over a socket, \
             and nothing proves that runtime runs in a container"
                .to_string(),
        ),
        _ if process.in_sandbox() => None,
        _ => Some(format!(
            "bridge.mode {mode:?} starts x0ne-runtime on this host, outside any container"
        )),
    }
}

/// The refusal for `why`, with the ways out.
pub fn refusal(why: &str) -> String {
    format!(
        "X0NE_REQUIRE_SANDBOX is set and {why}: the session is refused before its runtime \
         starts. To run sandboxed: --sandbox (REPL, -p, --tui and --matrix, after \
         `x0ne setup incus|podman`), a [bridge] sandbox_cmd that runs \
         `incus|podman|docker exec`, or \
         scripts/run-sandboxed.sh. Or unset X0NE_REQUIRE_SANDBOX."
    )
}

/// Under `X0NE_REQUIRE_SANDBOX`, `Err` for a bridge [`uncontained_reason`] names.
pub fn require_sandbox(mode: &str, sandbox_cmd: Option<&str>) -> anyhow::Result<()> {
    if !sandbox_required() {
        return Ok(());
    }
    match uncontained_reason(mode, sandbox_cmd, current()) {
        Some(why) => Err(anyhow::anyhow!(refusal(&why))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }
    fn markers(present: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |p| present.contains(&p)
    }

    #[test]
    fn no_declaration_is_the_host_even_in_a_container() {
        assert_eq!(
            detect_with(env(&[]), markers(&["/run/.containerenv"])),
            SandboxMode::Host
        );
    }

    #[test]
    fn a_declared_and_confirmed_sandbox_is_yolo_by_default() {
        let m = detect_with(env(&[("X0NE_SANDBOX", "incus")]), markers(&["/dev/incus"]));
        assert_eq!(
            m,
            SandboxMode::Yolo {
                runtime: "incus".into()
            }
        );
        assert!(m.yolo() && m.in_sandbox());
        assert_eq!(m.banner().as_deref(), Some("yolo · sandbox (incus)"));
        let m = detect_with(
            env(&[("X0NE_SANDBOX", "podman")]),
            markers(&["/run/.containerenv"]),
        );
        assert!(m.yolo());
    }

    #[test]
    fn the_operator_can_ask_for_prompts_back() {
        let m = detect_with(
            env(&[("X0NE_SANDBOX", "incus"), ("X0NE_SANDBOX_CONFIRM", "1")]),
            markers(&["/dev/incus"]),
        );
        assert_eq!(
            m,
            SandboxMode::Confirm {
                runtime: "incus".into()
            }
        );
        assert!(!m.yolo() && m.in_sandbox());
        assert_eq!(m.banner().as_deref(), Some("confirm · sandbox (incus)"));
    }

    #[test]
    fn a_declaration_without_a_container_is_not_trusted() {
        // `X0NE_SANDBOX=incus make run` on the host must not switch the gate off.
        let m = detect_with(env(&[("X0NE_SANDBOX", "incus")]), markers(&[]));
        assert!(matches!(m, SandboxMode::Unconfirmed { .. }), "{m:?}");
        assert!(!m.yolo() && !m.in_sandbox());
        let m = detect_with(env(&[("X0NE_SANDBOX", "chroot")]), markers(&["/dev/incus"]));
        assert!(!m.yolo(), "unknown runtime");
        assert!(m.banner().is_some_and(|b| b.contains("confirm")), "{m:?}");
    }

    #[test]
    fn open_egress_is_named_in_the_banner() {
        let open = env(&[("X0NE_SANDBOX", "podman"), ("X0NE_SANDBOX_NET", "open")]);
        let m = detect_with(&open, markers(&["/run/.containerenv"]));
        assert_eq!(
            egress_warning_with(&m, &open).as_deref(),
            Some("network OPEN (X0NE_SANDBOX_NET=open)")
        );
        let proxied = env(&[("X0NE_SANDBOX", "podman"), ("X0NE_SANDBOX_NET", "proxy")]);
        assert_eq!(egress_warning_with(&m, &proxied), None);
        // On the host the variable means nothing: no claim about the network.
        assert_eq!(egress_warning_with(&SandboxMode::Host, &open), None);
    }

    #[test]
    fn the_requirement_is_off_only_when_unset_empty_or_zero() {
        let required = |value: &str| {
            sandbox_required_with(|k| (k == "X0NE_REQUIRE_SANDBOX").then(|| value.to_string()))
        };
        assert!(!sandbox_required_with(env(&[])));
        for off in ["", " ", "0", " 0 "] {
            assert!(!required(off), "{off:?}");
        }
        for on in ["1", "true", "yes", "on", "false"] {
            assert!(required(on), "{on:?} must require the sandbox");
        }
    }

    #[test]
    fn a_runtime_started_on_the_host_is_uncontained() {
        for mode in ["stdio", "local", "auto", ""] {
            let why = uncontained_reason(mode, None, &SandboxMode::Host);
            assert!(
                why.as_deref().is_some_and(|w| w.contains("on this host")),
                "{mode:?}: {why:?}"
            );
        }
        let unconfirmed = SandboxMode::Unconfirmed {
            reason: "not in a container".into(),
        };
        assert!(uncontained_reason("stdio", None, &unconfirmed).is_some());
    }

    #[test]
    fn a_declared_and_confirmed_sandbox_contains_its_child_runtime() {
        for process in [
            SandboxMode::Yolo {
                runtime: "podman".into(),
            },
            SandboxMode::Confirm {
                runtime: "incus".into(),
            },
        ] {
            assert_eq!(uncontained_reason("stdio", None, &process), None);
            // A socket or a non-container command leaves the container.
            assert!(uncontained_reason("http", None, &process).is_some());
            assert!(uncontained_reason("sandbox", Some("ssh h rt"), &process).is_some());
        }
    }

    #[test]
    fn only_a_container_exec_command_is_a_sandbox_bridge() {
        for cmd in [
            "podman exec -i --env X0NE_ALLOW_LOOPBACK=1 x0ne-sandbox-podman /opt/x0ne-runtime",
            "incus exec x0ne-sandbox -- /opt/x0ne-runtime --mode stdio",
            "docker exec -i worker /opt/x0ne-runtime",
        ] {
            assert_eq!(
                uncontained_reason("sandbox", Some(cmd), &SandboxMode::Host),
                None,
                "{cmd}"
            );
        }
        for cmd in [
            None,
            Some(""),
            Some("ssh build-host incus exec x0ne-sandbox -- /opt/x0ne-runtime"),
            Some("/usr/local/bin/x0ne-runtime --mode stdio"),
            Some("podman run --rm img /opt/x0ne-runtime"),
            // Skipped by the detector, executed by the bridge.
            Some("A=1 podman exec x0ne-sandbox /opt/x0ne-runtime"),
        ] {
            assert!(
                uncontained_reason("sandbox", cmd, &SandboxMode::Host).is_some(),
                "{cmd:?}"
            );
        }
        assert!(uncontained_reason("http", None, &SandboxMode::Host).is_some());
    }

    #[test]
    fn the_refusal_names_the_variable_the_cause_and_the_ways_out() {
        let r = refusal("bridge.mode \"stdio\" starts x0ne-runtime on this host");
        assert!(
            r.starts_with("X0NE_REQUIRE_SANDBOX is set and bridge.mode"),
            "{r}"
        );
        for way in ["--sandbox", "sandbox_cmd", "run-sandboxed.sh", "unset"] {
            assert!(r.contains(way), "{way}: {r}");
        }
    }
}
