//! H4-2 (audit of 2026-10-30): `/until <cmd>` ran its command with `sh -c` in
//! the x0ne process, on the host, outside the policy and the sandbox every
//! other command goes through, and it ran it again after each turn. The real
//! binary; a recording stdio runtime stands in for the real one; the model is
//! never called.

#![cfg(unix)]

mod common;

use std::time::Duration;

/// The goal command reaches the runtime as a `RunCommand`, where the policy
/// and the isolation apply, and runs nowhere else. A shell is never dispensed
/// from the gate; with nobody at the keyboard it is refused and the goal is
/// dropped, not run.
#[test]
fn the_goal_command_goes_through_the_runtime_and_never_the_host() {
    let model = common::Model::start("ok");
    let fx = common::Fixture::new(&model);
    let work = fx.path("work");
    let seen = fx.path("runtime-seen");
    // Records every RunCommand it is handed and reports each as failed, so a
    // goal is never "already passing".
    std::fs::write(
        fx.path("bin/stdio-runtime"),
        format!(
            "#!/bin/sh\nwhile read -r line; do\n  case \"$line\" in\n    \
             *PolicyCheck*) echo '{{\"success\":true,\"output\":{{\"allowed\":true}}}}' ;;\n    \
             *RunCommand*) printf '%s\\n' \"$line\" >> '{}'; echo '{{\"success\":true,\"output\":{{\"kind\":\"command_output\",\"exit_code\":1,\"stdout\":\"\",\"stderr\":\"not yet\"}}}}' ;;\n    \
             *) echo '{{\"success\":true,\"output\":{{\"actions\":[]}}}}' ;;\n  esac\ndone\n",
            seen.display()
        ),
    )
    .expect("runtime");
    // `touch` inside the project is dispensed from the human gate by a trust
    // root, so the first goal goes straight to the runtime.
    let cfg = fx.path("until.toml");
    std::fs::write(
        &cfg,
        format!(
            "{}\n[trust]\nroots = [{:?}]\nworkdir_programs = [\"touch\"]\n",
            std::fs::read_to_string(fx.path("chosen.toml")).expect("chosen.toml"),
            work.display()
        ),
    )
    .expect("config");

    let out = fx.run_with_input(
        &work,
        &["-c", cfg.to_str().expect("utf-8 path")],
        "/until touch canary\n/until sh -c 'touch canary2'\n/quit\n",
        Duration::from_secs(90),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !work.join("canary").exists(),
        "the goal command ran on the host\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(
        !work.join("canary2").exists(),
        "the shell goal ran on the host\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    let seen = std::fs::read_to_string(&seen).unwrap_or_default();
    assert!(
        seen.contains("\"RunCommand\"") && seen.contains("\"touch\"") && seen.contains("canary"),
        "the goal command never reached the runtime\n--- runtime saw\n{seen}\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    assert!(
        !seen.contains("canary2"),
        "a shell reached the runtime with nobody to approve it\n--- runtime saw\n{seen}"
    );
    assert!(
        stdout.contains("goal refused") || stderr.contains("goal refused"),
        "the refused goal was not reported as refused\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
}
