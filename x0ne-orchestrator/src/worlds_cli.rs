//! CLI helpers for the parallel-worlds feature.
//!
//! Two entry points:
//!
//! - [`render_live`] — inline live render of the current worlds registry,
//!   drawn in place below the prompt. Exits when every world reaches a
//!   terminal state (or the user hits Ctrl+C).
//! - [`spawn_worlds`] — `/spawn N <prompt>`: start N real worlds through the
//!   session, on the same path an LLM-emitted `SpawnWorld` takes.
//!
//! Progress reaches the process-global `worlds_registry::global()` from the
//! world task itself, so every observer (this renderer, the TUI worlds pane)
//! sees exactly the same stream of events.

use colored::Colorize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::agent::session::CliAgentSession;
use crate::interrupt;
use crate::worlds_registry;
use crate::worlds_view::{self, WorldEvent};

/// When `auto_worlds_view` is true (default), if any parallel world is still
/// active after the parent agent turn, open [`render_live`]. Disable with
/// `X0NE_AUTO_WORLDS=0` or `[ui] auto_worlds_view = false`.
pub async fn maybe_auto_render_parallel_worlds(
    config: &crate::config::Config,
) -> anyhow::Result<()> {
    if !config.ui.auto_worlds_view {
        return Ok(());
    }
    if worlds_registry::global().active_count() == 0 {
        return Ok(());
    }
    render_live().await
}

/// Live `run_renderer` view (crossterm).
///
/// Exits when every world is terminal or the event channel closes; Ctrl+C
/// detaches the forwarder.
pub async fn render_live() -> anyhow::Result<()> {
    let ids = worlds_registry::global().ids();
    if ids.is_empty() {
        println!(
            "  {}  no parallel worlds have been spawned yet",
            "⫸".truecolor(0, 200, 220),
        );
        println!(
            "     start some with {}",
            "/spawn 2 <prompt>".truecolor(120, 200, 255).bold(),
        );
        return Ok(());
    }

    println!(
        "  {}  live worlds view — {} total, {} active",
        "⫸".truecolor(0, 200, 220).bold(),
        ids.len().to_string().truecolor(0, 200, 220).bold(),
        worlds_registry::global()
            .active_count()
            .to_string()
            .truecolor(0, 200, 220),
    );
    println!(
        "  {}  press Ctrl+C to detach (workers keep running)",
        " ".to_string().dimmed(),
    );
    println!();

    // Bridge registry → run_renderer. The renderer expects an mpsc receiver;
    // we spawn a forwarder that pulls from our broadcast subscription and
    // replays the past events first, then forwards live events.
    let (fwd_tx, fwd_rx) = mpsc::unbounded_channel::<WorldEvent>();
    let (replay, mut rx) = worlds_registry::global().subscribe();

    let forwarder = tokio::spawn(async move {
        for ev in replay {
            if fwd_tx.send(ev).is_err() {
                return;
            }
        }
        loop {
            tokio::select! {
                biased;
                _ = interrupt::global().wait() => {
                    // User detaches — drop the sender to unblock the renderer.
                    return;
                }
                recv = rx.recv() => {
                    match recv {
                        Ok(ev) => {
                            if fwd_tx.send(ev).is_err() { return; }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // We lagged behind; surface a synthetic note by
                            // skipping — the renderer survives missing events
                            // as long as terminal events eventually arrive.
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
        }
    });

    let res = worlds_view::run_renderer(ids, fwd_rx).await;
    forwarder.abort();
    res.map_err(anyhow::Error::from)
}

/// What `/spawn N <prompt>` asked for, before the session caps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    pub count: usize,
    pub prompt: String,
}

/// The usage line a malformed `/spawn` gets. A world with no task has
/// nothing to run, so there is no default prompt.
pub const SPAWN_USAGE: &str = "usage: /spawn <N> <prompt>  — start N parallel worlds on <prompt>";

/// Ceiling when the session sets no `max_children`: the interactive default
/// is unbounded for the model, but an operator typo must not open 99 runtimes.
pub const DEFAULT_MAX_WORLDS: usize = 4;

/// Parse the argument of `/spawn`. `N` is required and must be at least 1;
/// everything after it is the prompt, and an empty prompt is refused.
pub fn parse_spawn(arg: &str) -> Result<SpawnRequest, String> {
    let arg = arg.trim();
    let (n_str, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let count: usize = n_str.parse().map_err(|_| SPAWN_USAGE.to_string())?;
    if count == 0 {
        return Err(SPAWN_USAGE.to_string());
    }
    let prompt = rest.trim();
    if prompt.is_empty() {
        return Err(SPAWN_USAGE.to_string());
    }
    Ok(SpawnRequest {
        count,
        prompt: prompt.to_string(),
    })
}

/// How many worlds may start now: the requested count, bounded by the room
/// left under the cap once the children already registered are counted.
pub fn clamp_count(requested: usize, cap: Option<usize>, registered: usize) -> usize {
    let cap = cap.unwrap_or(DEFAULT_MAX_WORLDS);
    requested.min(cap.saturating_sub(registered))
}

/// `world-<k>` ids, process-wide, skipping any id the session already holds.
fn fresh_world_id(registry: &crate::sysaicalls::AgentRegistry) -> String {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(1);
    loop {
        let k = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("world-{k}");
        if registry.status_of(&id).is_none() {
            return id;
        }
    }
}

/// Start the requested worlds through the session, one `SpawnWorld` action
/// each. The action crosses the same gates as one the model emits: policy,
/// confirmation, the child cap, then `SessionWorlds::spawn_detached`.
/// Returns one envelope per attempted world.
pub async fn spawn_worlds(session: &mut CliAgentSession, request: &SpawnRequest) -> Vec<Value> {
    let registered = session.registry.count();
    let n = clamp_count(request.count, session.max_children, registered);
    let cap = session.max_children.unwrap_or(DEFAULT_MAX_WORLDS);
    if n == 0 {
        print_error(&format!(
            "no room for a new world: {registered} registered, cap {cap} (/worlds-reset does not free them)"
        ));
        return Vec::new();
    }
    if n < request.count {
        println!(
            "  {}  {} requested, {} started (cap {})",
            "⫸".truecolor(0, 200, 220),
            request.count,
            n,
            cap
        );
    }

    // Equal shares of what the session budget has left, so the N worlds
    // together stay under `/budget`. No budget: no dollar slice.
    let cap = session.state.budget.limit_usd;
    let share = if cap > 0.0 {
        let left = cap - session.state.budget.spent_usd - session.worlds_budget.committed();
        Some(left / n as f64)
    } else {
        None
    };
    if let Some(s) = share {
        println!(
            "  {}  budget: ${:.4} per world, ${:.2} session cap",
            "⫸".truecolor(0, 200, 220),
            s.max(0.0),
            cap
        );
    }

    let mut envelopes = Vec::with_capacity(n);
    for _ in 0..n {
        let id = fresh_world_id(&session.registry);
        let mut action = json!({"type": "SpawnWorld", "id": id, "task": request.prompt});
        if let Some(s) = share.filter(|s| *s > 0.0) {
            action["budget_usd"] = json!(s);
        }
        let envelope = session.run_operator_action(&action).await;
        if envelope["success"] == true {
            println!(
                "  {}  {} started — {}",
                "⫸".truecolor(0, 200, 220).bold(),
                id.truecolor(0, 200, 220).bold(),
                request.prompt.dimmed()
            );
        } else {
            let why = envelope["error"]["message"]
                .as_str()
                .unwrap_or("refused")
                .to_string();
            print_error(&format!("{id}: {why}"));
        }
        envelopes.push(envelope);
    }
    println!(
        "     watch with {}",
        "/worlds".truecolor(120, 200, 255).bold(),
    );
    envelopes
}

/// `/worlds [watch | diff [n] | pick <n> | drop]`. Returns the text to print.
/// `watch` is handled by the caller (it takes the terminal).
pub fn worlds_command(arg: &str) -> String {
    use crate::worlds_workspace::{self as ws, Status};
    let mut parts = arg.split_whitespace();
    let sub = parts.next().unwrap_or("");
    let target = parts.next().map(ws::resolve_id);
    match sub {
        "" | "ls" | "list" => {
            let entries = ws::ledger().all();
            let mut out = ws::render_table(&entries);
            if entries.iter().any(|e| e.status == Status::Running) {
                out.push_str("live view: /worlds watch\n");
            }
            if entries.iter().any(|e| e.status != Status::Running) {
                out.push_str("compare: /worlds diff · apply one: /worlds pick <n> · clean up: /worlds drop\n");
            }
            out
        }
        "diff" => {
            let entries: Vec<_> = ws::ledger()
                .all()
                .into_iter()
                .filter(|e| target.as_deref().is_none_or(|t| t == e.id))
                .collect();
            if entries.is_empty() {
                return match target {
                    Some(t) => format!("no world {t}\n"),
                    None => "no worlds to compare\n".into(),
                };
            }
            let mut out = String::new();
            for e in entries {
                let status = match &e.status {
                    Status::Running => "running".to_string(),
                    Status::Succeeded => "ok".to_string(),
                    Status::Failed(_) => "failed".to_string(),
                };
                let cost = e
                    .cost_usd
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "cost unknown".into());
                out.push_str(&format!("── {} · {} · {}\n", e.id, status, cost));
                let Some(dir) = &e.dir else {
                    out.push_str("   (no own dir: nothing to compare)\n");
                    continue;
                };
                let body = if target.is_some() {
                    ws::diff(dir)
                } else {
                    ws::diffstat(dir)
                };
                match body {
                    Ok(t) if t.trim().is_empty() => out.push_str("   no changes\n"),
                    Ok(t) => out.push_str(&t),
                    Err(err) => out.push_str(&format!("   diff failed: {err:#}\n")),
                }
            }
            if target.is_none() {
                out.push_str("full diff of one world: /worlds diff <n>\n");
            }
            out
        }
        "pick" => {
            let Some(id) = target else {
                return "usage: /worlds pick <n>\n".into();
            };
            match ws::pick_world(&id) {
                Ok((dir, files)) => format!(
                    "applied {id}: {files} file(s) into {}\n",
                    dir.base.display()
                ),
                Err(e) => format!("pick refused: {e:#}\n"),
            }
        }
        "drop" => {
            let (dropped, kept, errors) = ws::drop_finished();
            let mut out = format!("dropped {} world(s)", dropped.len());
            if !kept.is_empty() {
                out.push_str(&format!(", kept {} still running", kept.len()));
            }
            out.push('\n');
            for e in errors {
                out.push_str(&format!("drop failed: {e}\n"));
            }
            out
        }
        _ => "usage: /worlds [watch | diff [n] | pick <n> | drop]\n".into(),
    }
}

fn print_error(msg: &str) {
    println!("  {}  {}", "✗".truecolor(200, 60, 60), msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::LlmBackend;
    use crate::bridge::Bridge;
    use crate::config::Config;
    use crate::display_sink::NullDisplaySink;
    use std::sync::Arc;
    use std::time::Duration;

    // The world tasks publish into the process-global worlds registry, which
    // other test modules also reset. One lock keeps the counts honest.
    static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn spawn_without_a_prompt_is_refused_with_usage() {
        for bad in ["", "3", "   3   ", "abc", "0 do it", "fix the build"] {
            assert_eq!(parse_spawn(bad), Err(SPAWN_USAGE.to_string()), "{bad:?}");
        }
        assert_eq!(
            parse_spawn(" 2   fix the build  "),
            Ok(SpawnRequest {
                count: 2,
                prompt: "fix the build".into()
            })
        );
    }

    #[test]
    fn spawn_count_is_clamped_to_the_session_cap() {
        assert_eq!(clamp_count(99, Some(3), 0), 3);
        assert_eq!(clamp_count(2, Some(3), 0), 2);
        assert_eq!(clamp_count(99, None, 0), DEFAULT_MAX_WORLDS);
        // Children already registered use up the room.
        assert_eq!(clamp_count(5, Some(3), 2), 1);
        assert_eq!(clamp_count(1, Some(3), 3), 0);
        assert_eq!(clamp_count(1, Some(3), 7), 0);
    }

    struct QuietBackend;

    #[async_trait::async_trait]
    impl LlmBackend for QuietBackend {
        async fn send(
            &self,
            _system: &str,
            _messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            Ok(crate::api::ApiResponse {
                content: "world done".into(),
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "quiet".into()
        }
    }

    struct OkBridge;

    #[async_trait::async_trait]
    impl Bridge for OkBridge {
        async fn send_action(&mut self, _action: &Value) -> anyhow::Result<Value> {
            Ok(json!({"success": true, "output": {}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct CountingRuntime(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait::async_trait]
    impl crate::agent::world::WorldRuntime for CountingRuntime {
        async fn open(
            &self,
            _ctx: &crate::agent::world::WorldContext,
        ) -> anyhow::Result<Box<dyn Bridge>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(OkBridge))
        }
    }

    /// `/spawn 5 …` under a cap of 2 starts exactly two real worlds, each on
    /// its own runtime, registered where `WaitAgent` and `/worlds` look.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_worlds_starts_real_worlds_through_the_session() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worlds_registry::global().reset();
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut session = CliAgentSession::new(
            Config::default(),
            Arc::new(QuietBackend),
            "SYS",
            Some(Box::new(OkBridge)),
            Box::new(NullDisplaySink),
        );
        session.world_runtime = Arc::new(CountingRuntime(Arc::clone(&opened)));
        session.max_children = Some(2);

        let request = SpawnRequest {
            count: 5,
            prompt: "say hello".into(),
        };
        let envelopes = spawn_worlds(&mut session, &request).await;
        assert_eq!(envelopes.len(), 2);
        let ids: Vec<String> = envelopes
            .iter()
            .map(|e| {
                assert_eq!(e["success"], true, "{e}");
                assert_eq!(e["output"]["status"], "running");
                assert_eq!(e["output"]["task"], "say hello");
                e["output"]["id"].as_str().unwrap().to_string()
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert_eq!(session.registry.count(), 2);
        assert_eq!(worlds_registry::global().total(), 2);

        for id in &ids {
            let result = crate::sysaicalls::execute_wait_agent(
                &session.registry,
                &json!({"id": id, "timeout_secs": 10}),
            )
            .await
            .unwrap();
            assert_eq!(result["output"]["response"], "world done", "{result}");
        }
        assert_eq!(opened.load(std::sync::atomic::Ordering::SeqCst), 2);

        // At the cap, a further request is refused before any action runs.
        let more = spawn_worlds(&mut session, &request).await;
        assert!(more.is_empty());

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while worlds_registry::global().active_count() > 0 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(worlds_registry::global().active_count(), 0);
        worlds_registry::global().reset();
    }

    // ── Real worlds, end to end, on a deterministic mock model ──────────

    use crate::worlds_workspace::{self as ws, Status};

    /// The world's id and dir, read off the task the world was given.
    fn world_of(messages: &[crate::conversation::Message]) -> (String, String) {
        let text = messages
            .iter()
            .map(|m| m.content.clone())
            .collect::<Vec<_>>()
            .join("\n");
        let id = text
            .split("parallel world `")
            .nth(1)
            .and_then(|r| r.split('`').next())
            .unwrap_or("")
            .to_string();
        let dir = text
            .split("Your working directory is ")
            .nth(1)
            .and_then(|r| r.split(", an isolated").next())
            .unwrap_or("")
            .to_string();
        (id, dir)
    }

    /// Three worlds; the one with the highest number writes where the runtime
    /// refuses. Each writes its own `is_prime.py`, then says done.
    struct ThreeWorlds {
        seen: std::sync::Mutex<Vec<String>>,
        calls: std::sync::Mutex<std::collections::HashMap<String, usize>>,
    }

    #[async_trait::async_trait]
    impl LlmBackend for ThreeWorlds {
        async fn send(
            &self,
            _system: &str,
            messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            let (id, dir) = world_of(messages);
            assert!(!dir.is_empty(), "a world was not told its dir");
            {
                let mut seen = self.seen.lock().unwrap();
                if !seen.contains(&id) {
                    seen.push(id.clone());
                }
            }
            // Wait until the three worlds are known, so the failing one is
            // decided by number and not by scheduling.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while self.seen.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let mut ids = self.seen.lock().unwrap().clone();
            ids.sort_by_key(|i| i.trim_start_matches("world-").parse::<usize>().unwrap_or(0));
            let failing = ids.last() == Some(&id);
            let n = {
                let mut calls = self.calls.lock().unwrap();
                let c = calls.entry(id.clone()).or_insert(0);
                *c += 1;
                *c
            };
            let content = if n == 1 {
                let file = if failing { "fail.py" } else { "is_prime.py" };
                crate::agent::session::tests::action_reply(json!({
                    "type": "WriteFile",
                    "path": format!("{dir}/{file}"),
                    "content": format!("# approach of {id}\n"),
                }))
            } else {
                format!("{id} done")
            };
            Ok(crate::api::ApiResponse {
                content,
                input_tokens: 100,
                output_tokens: 20,
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            "mock/three-worlds".into()
        }
    }

    /// A runtime that really writes, and refuses `fail.py`.
    struct FsBridge;

    #[async_trait::async_trait]
    impl Bridge for FsBridge {
        async fn send_action(&mut self, action: &Value) -> anyhow::Result<Value> {
            if action["type"] == "WriteFile" {
                let path = action["path"].as_str().unwrap_or("");
                if path.ends_with("fail.py") {
                    return Ok(json!({"success": false, "error": {"message": "disk full"}}));
                }
                std::fs::write(path, action["content"].as_str().unwrap_or(""))?;
            }
            Ok(json!({"success": true, "output": {}}))
        }
        async fn health_check(&mut self) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    struct FsRuntime(Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>);

    #[async_trait::async_trait]
    impl crate::agent::world::WorldRuntime for FsRuntime {
        async fn open(
            &self,
            ctx: &crate::agent::world::WorldContext,
        ) -> anyhow::Result<Box<dyn Bridge>> {
            let dir = ctx
                .work_dir
                .clone()
                .ok_or_else(|| anyhow::anyhow!("a world opened its runtime without its own dir"))?;
            self.0.lock().unwrap().push(dir);
            Ok(Box::new(FsBridge))
        }
    }

    fn git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let base = t.path().canonicalize().unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&base)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .status()
                .unwrap();
            assert!(st.success());
        };
        git(&["init", "-q"]);
        std::fs::write(base.join("README"), "project\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "init"]);
        (t, base)
    }

    fn worlds_session(
        base: &std::path::Path,
        backend: Arc<dyn LlmBackend>,
        config: Config,
    ) -> (
        CliAgentSession,
        Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
    ) {
        let mut config = config;
        // The operator trusts the project dir: writes under it need no prompt.
        config.trust.roots = vec![base.display().to_string()];
        let opened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = CliAgentSession::new(
            config,
            backend,
            "SYS",
            Some(Box::new(OkBridge)),
            Box::new(NullDisplaySink),
        );
        session.world_runtime = Arc::new(FsRuntime(Arc::clone(&opened)));
        session.worlds_base = Some(base.to_path_buf());
        (session, opened)
    }

    async fn wait_all(session: &CliAgentSession, envelopes: &[Value]) -> Vec<String> {
        let mut ids = Vec::new();
        for e in envelopes {
            assert_eq!(e["success"], true, "{e}");
            let id = e["output"]["id"].as_str().unwrap().to_string();
            crate::sysaicalls::execute_wait_agent(
                &session.registry,
                &json!({"id": id, "timeout_secs": 20}),
            )
            .await
            .unwrap();
            ids.push(id);
        }
        // The ledger is written after the registry; give the task its turn.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ids
            .iter()
            .any(|i| ws::ledger().get(i).map(|e| e.status) == Some(Status::Running))
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ids
    }

    /// 3 worlds, 1 failing: each in its own worktree, `/worlds` shows the real
    /// outcome, `pick` applies the chosen world's diff, `drop` removes the
    /// worktrees.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn three_real_worlds_one_fails_pick_applies_the_chosen_diff_drop_cleans_up() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worlds_registry::global().reset();
        let _ = ws::drop_finished();
        let (_t, base) = git_repo();
        let backend = Arc::new(ThreeWorlds {
            seen: Default::default(),
            calls: Default::default(),
        });
        let (mut session, opened) = worlds_session(&base, backend, Config::default());

        let envelopes = spawn_worlds(
            &mut session,
            &SpawnRequest {
                count: 3,
                prompt: "write is_prime".into(),
            },
        )
        .await;
        assert_eq!(envelopes.len(), 3);
        let mut ids = wait_all(&session, &envelopes).await;
        ids.sort_by_key(|i| i.trim_start_matches("world-").parse::<usize>().unwrap());

        // Isolation: three runtimes, three distinct dirs, none of them the base.
        let dirs = opened.lock().unwrap().clone();
        assert_eq!(dirs.len(), 3);
        let unique: std::collections::BTreeSet<_> = dirs.iter().collect();
        assert_eq!(unique.len(), 3);
        assert!(dirs.iter().all(|d| d != &base && d.starts_with(&base)));
        assert!(
            !base.join("is_prime.py").exists(),
            "a world wrote into the base"
        );

        let rows: Vec<_> = ids.iter().map(|i| ws::ledger().get(i).unwrap()).collect();
        assert_eq!(rows[0].status, Status::Succeeded);
        assert_eq!(rows[1].status, Status::Succeeded);
        assert!(
            matches!(rows[2].status, Status::Failed(_)),
            "{:?}",
            rows[2].status
        );
        for r in &rows {
            assert!(r.tokens > 0, "{} reported no tokens", r.id);
            assert!(r.actions >= 1);
        }

        let table = worlds_command("");
        let status_of = |id: &str| {
            table
                .lines()
                .find(|l| l.starts_with(&format!("{id} ")))
                .and_then(|l| l.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(status_of(&ids[0]), "ok", "{table}");
        assert_eq!(status_of(&ids[1]), "ok", "{table}");
        assert_eq!(status_of(&ids[2]), "failed", "{table}");
        assert!(table.contains("without a price"), "{table}");
        assert!(!table.contains("running"), "{table}");
        // The mock model has no price: the table says so instead of $0.
        assert!(table.contains("unknown"), "{table}");

        let diff = worlds_command("diff");
        for id in &ids {
            assert!(diff.contains(id.as_str()), "{diff}");
        }
        assert!(diff.contains("is_prime.py"), "{diff}");

        // Pick the second world: its file, its content, in the base.
        let n = ids[1].trim_start_matches("world-").to_string();
        let picked = worlds_command(&format!("pick {n}"));
        assert!(picked.starts_with("applied"), "{picked}");
        assert_eq!(
            std::fs::read_to_string(base.join("is_prime.py")).unwrap(),
            format!("# approach of {}\n", ids[1])
        );

        let dropped = worlds_command("drop");
        assert!(dropped.contains("dropped"), "{dropped}");
        assert!(dirs.iter().all(|d| !d.exists()), "a worktree survived drop");
        let wt = std::process::Command::new("git")
            .arg("-C")
            .arg(&base)
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&wt.stdout).lines().count(), 1);
        assert!(ids.iter().all(|i| ws::ledger().get(i).is_none()));
        worlds_registry::global().reset();
    }

    /// Keeps asking for actions: only a budget can stop it.
    struct Endless {
        name: &'static str,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmBackend for Endless {
        async fn send(
            &self,
            _system: &str,
            messages: &[crate::conversation::Message],
        ) -> anyhow::Result<crate::api::ApiResponse> {
            let k = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (_, dir) = world_of(messages);
            Ok(crate::api::ApiResponse {
                content: crate::agent::session::tests::action_reply(json!({
                    "type": "WriteFile",
                    "path": format!("{dir}/step{k}.txt"),
                    "content": "x",
                })),
                input_tokens: 1000,
                output_tokens: 200,
                ..Default::default()
            })
        }
        fn display_name(&self) -> String {
            self.name.into()
        }
    }

    /// `/budget 0.006` and two worlds: each gets $0.003, and a world that
    /// spends it is cut. Haiku rates: 1000 in + 200 out = $0.002 a call.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_exceeded_budget_cuts_the_world_and_worlds_share_the_session_cap() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worlds_registry::global().reset();
        let _ = ws::drop_finished();
        let (_t, base) = git_repo();
        let backend = Arc::new(Endless {
            name: "anthropic/claude-haiku-4-5",
            calls: Default::default(),
        });
        let mut config = Config::default();
        config.cost.budget_usd_limit = 0.006;
        let (mut session, _) = worlds_session(&base, backend.clone(), config);

        let envelopes = spawn_worlds(
            &mut session,
            &SpawnRequest {
                count: 2,
                prompt: "loop".into(),
            },
        )
        .await;
        let ids = wait_all(&session, &envelopes).await;
        let mut total = 0.0;
        for id in &ids {
            let row = ws::ledger().get(id).unwrap();
            assert!((row.budget_usd - 0.003).abs() < 1e-9, "{row:?}");
            match &row.status {
                Status::Failed(why) => assert!(why.contains("budget"), "{why}"),
                other => panic!("{id} was not cut: {other:?}"),
            }
            let cost = row.cost_usd.expect("Haiku is priced");
            assert!((0.003..=0.003 + 0.0021).contains(&cost), "{cost}");
            total += cost;
        }
        // Without the budget the model would have run 100 actions per world.
        assert!(backend.calls.load(std::sync::atomic::Ordering::SeqCst) <= 6);
        // The cap was committed: a third world finds nothing left.
        let more = spawn_worlds(
            &mut session,
            &SpawnRequest {
                count: 1,
                prompt: "loop".into(),
            },
        )
        .await;
        assert!(
            more.is_empty() || more[0]["success"] == false,
            "a world started past the session cap: {more:?}"
        );
        assert!(total > 0.0);
        let _ = ws::drop_finished();
        worlds_registry::global().reset();
    }

    /// `cost.world_max_tokens`: the call after the cap is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_token_budget_cuts_a_world() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worlds_registry::global().reset();
        let _ = ws::drop_finished();
        let (_t, base) = git_repo();
        let backend = Arc::new(Endless {
            name: "mock/endless",
            calls: Default::default(),
        });
        let mut config = Config::default();
        config.cost.world_max_tokens = 2000;
        let (mut session, _) = worlds_session(&base, backend.clone(), config);
        let envelopes = spawn_worlds(
            &mut session,
            &SpawnRequest {
                count: 1,
                prompt: "loop".into(),
            },
        )
        .await;
        let ids = wait_all(&session, &envelopes).await;
        let row = ws::ledger().get(&ids[0]).unwrap();
        match &row.status {
            Status::Failed(why) => assert!(why.contains("token budget"), "{why}"),
            other => panic!("not cut: {other:?}"),
        }
        // 1200 tokens a call: the second call passes the cap, the third is refused.
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = ws::drop_finished();
        worlds_registry::global().reset();
    }

    struct BrokenRuntime;

    #[async_trait::async_trait]
    impl crate::agent::world::WorldRuntime for BrokenRuntime {
        async fn open(
            &self,
            _ctx: &crate::agent::world::WorldContext,
        ) -> anyhow::Result<Box<dyn Bridge>> {
            Err(anyhow::anyhow!("no runtime"))
        }
    }

    /// Nothing is shown that did not run: no worlds, no rows; a world whose
    /// runtime never opened is a failure with no actions, never a success.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_result_is_shown_without_a_real_execution() {
        let _g = REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        worlds_registry::global().reset();
        let _ = ws::drop_finished();
        assert!(worlds_command("").starts_with("no worlds"));
        assert!(worlds_command("diff").contains("no worlds"));

        let (_t, base) = git_repo();
        let backend = Arc::new(Endless {
            name: "mock/never-called",
            calls: Default::default(),
        });
        let (mut session, _) = worlds_session(&base, backend.clone(), Config::default());
        session.world_runtime = Arc::new(BrokenRuntime);
        let envelopes = spawn_worlds(
            &mut session,
            &SpawnRequest {
                count: 1,
                prompt: "x".into(),
            },
        )
        .await;
        let ids = wait_all(&session, &envelopes).await;
        let row = ws::ledger().get(&ids[0]).unwrap();
        assert!(matches!(row.status, Status::Failed(_)));
        assert_eq!((row.actions, row.tokens), (0, 0));
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let table = worlds_command("");
        assert!(
            table.contains(" failed ") && !table.contains(" ok "),
            "{table}"
        );
        let _ = ws::drop_finished();
        worlds_registry::global().reset();
    }
}
