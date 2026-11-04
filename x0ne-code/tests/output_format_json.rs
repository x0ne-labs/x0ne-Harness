//! `x0ne -p … --output-format json`: stdout carries the JSON object and
//! nothing else, so a script can parse it. Measured 2026-10-27, before the fix:
//! the rendered answer was printed on stdout before the JSON.

#![cfg(unix)]

mod common;

use common::{Fixture, Model};

#[test]
fn json_output_is_the_only_thing_on_stdout() {
    let model = Model::start("pong-json-1084");
    let fx = Fixture::new(&model);
    let out = fx.one_shot(&fx.path("work"), "hello", &["--output-format", "json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout is not one JSON value ({e}):\n{stdout}\nstderr: {stderr}")
    });
    assert_eq!(parsed["response"], "pong-json-1084", "{parsed}");
    assert_eq!(stdout.trim().lines().count(), 1, "{stdout}");
}

/// The text format is unchanged: the answer is on stdout.
#[test]
fn text_output_still_prints_the_answer_on_stdout() {
    let model = Model::start("pong-text-1084");
    let fx = Fixture::new(&model);
    let out = fx.one_shot(&fx.path("work"), "hello", &[]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("pong-text-1084"), "{stdout}");
}

/// Stdin and stderr on a terminal, stdout on a pipe: nobody reads the JSON,
/// so a gated action is refused, never asked. Review of 2026-10-27: once fd 1
/// pointed at stderr it was a terminal, and a WriteFile waited for a key.
#[test]
fn json_mode_with_stdout_on_a_pipe_never_prompts() {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let model = Model::start(
        "```json\n{\"type\":\"WriteFile\",\"path\":\"out.txt\",\"content\":\"x\"}\n```",
    );
    let fx = Fixture::new(&model);
    // A policy in the directory: the first-run wizard, which asks whenever
    // stdin is a terminal, stays out of this test.
    std::fs::write(fx.path("work/x0ne-runtime.toml"), "").expect("policy");
    let pty = portable_pty::native_pty_system()
        .openpty(portable_pty::PtySize::default())
        .expect("pty");
    let tty_path = pty.master.tty_name().expect("tty name");
    let tty = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tty_path)
            .expect("open the terminal")
    };
    let cfg = fx.path("chosen.toml").display().to_string();
    let mut cmd = fx.command(
        &fx.path("work"),
        &["-c", &cfg, "-p", "write it", "--output-format", "json"],
    );
    cmd.stdin(tty())
        .stdout(std::process::Stdio::piped())
        .stderr(tty());
    let mut child = cmd.spawn().expect("spawn x0ne");
    drop(cmd);
    drop(pty.slave);

    let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let sink = std::sync::Arc::clone(&screen);
    let mut reader = pty.master.try_clone_reader().expect("reader");
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });
    let prompts = ["press 1/2", "allow once", "[y/N]", "choose ❯"];
    let shown = || String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() > deadline || prompts.iter().any(|p| shown().contains(p)) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut stdout = String::new();
    let _ = child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout);
    std::thread::sleep(Duration::from_millis(200));
    let screen = shown();
    for p in prompts {
        assert!(!screen.contains(p), "asked on the terminal:\n{screen}");
    }
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(2),
        "stdout: {stdout}\nterminal: {screen}"
    );
    assert!(
        !fx.path("work/out.txt").exists(),
        "the refused write happened"
    );
}

/// The JSON names the endpoint by its origin: a path or a query (`?key=`)
/// may hold a secret. Review of 2026-10-27: the whole URL was there.
#[test]
fn json_model_names_the_endpoint_origin_only() {
    let model = Model::start("pong-origin-1084");
    let fx = Fixture::new(&model);
    let canary = "QUERY-CANARY-1084";
    let cfg = fx.path("chosen.toml");
    let text = std::fs::read_to_string(&cfg).expect("config");
    let url = model.url();
    std::fs::write(
        &cfg,
        text.replace(
            &format!("base_url = \"{url}\""),
            &format!("base_url = \"{url}/v1?key={canary}\""),
        ),
    )
    .expect("config");
    let out = fx.one_shot(&fx.path("work"), "hello", &["--output-format", "json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not one JSON value ({e}):\n{stdout}"));
    assert_eq!(
        parsed["model"],
        format!("openai-compat/chosen-model @ {url}"),
        "{parsed}"
    );
    assert!(!stdout.contains(canary), "{stdout}");
}
