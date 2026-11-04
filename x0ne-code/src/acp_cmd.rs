//! `x0ne --acp`: this process becomes an ACP agent on stdin/stdout.
//! Nothing else may write to stdout here: it carries the protocol.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result};
use x0ne_orchestrator::acp::{self, SessionFactory};
use x0ne_orchestrator::agent::session::CliAgentSession;
use x0ne_orchestrator::api::{self, LlmBackend};
use x0ne_orchestrator::config::Config;
use x0ne_orchestrator::display_sink::DisplaySink;

struct Factory {
    config: Config,
    llm: Arc<dyn LlmBackend>,
    cwd: PathBuf,
}

impl SessionFactory for Factory {
    fn create(
        &self,
        cwd: PathBuf,
        sink: Box<dyn DisplaySink>,
    ) -> Pin<Box<dyn Future<Output = Result<CliAgentSession>> + Send + '_>> {
        Box::pin(async move {
            // The runtime confines to the directory it starts in; a session
            // elsewhere would silently work on the wrong tree.
            anyhow::ensure!(
                acp::same_dir(&cwd, &self.cwd),
                "this x0ne works in {}; start it from {} to work there",
                self.cwd.display(),
                cwd.display()
            );
            let mut bridge = x0ne_orchestrator::bridge::create_bridge_for(&self.config.bridge)
                .await
                .context("cannot start x0ne-runtime")?;
            bridge
                .health_check()
                .await
                .context("x0ne-runtime health check")?;
            let actions = bridge
                .send_action(&serde_json::json!({"type": "ListActions"}))
                .await
                .ok()
                .and_then(|r| r["output"]["actions"].as_array().cloned());
            if let Some(a) = &actions {
                x0ne_orchestrator::parser::init_known_actions(a);
            }
            let prompt = x0ne_orchestrator::prompt::build_system_prompt_dynamic(
                &self.config,
                false,
                Some("ACP client (editor)"),
                actions.as_deref(),
            );
            let mut agent = CliAgentSession::new(
                self.config.clone(),
                Arc::clone(&self.llm),
                prompt,
                Some(bridge),
                sink,
            );
            agent.discovered_actions = actions;
            let journal = x0ne_orchestrator::session::SharedJournal::new()
                .await
                .context("cannot open the session journal")?;
            agent.journal = Some(journal.handle());
            Ok(agent)
        })
    }
}

pub async fn run(config: Config) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    let key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|s| !s.is_empty());
    let llm: Arc<dyn LlmBackend> = api::create_backend(&config.llm, key)
        .map(Arc::from)
        .with_context(|| format!("no LLM backend (provider = {})", config.llm.provider))?;
    x0ne_orchestrator::model_keys::announce(&config.llm);
    acp::serve_stdio(Arc::new(Factory { config, llm, cwd })).await
}
