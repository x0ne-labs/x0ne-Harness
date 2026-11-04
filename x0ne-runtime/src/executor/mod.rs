use crate::action::Action;
use crate::composite::CompositeStore;
use crate::config::AppConfig;
use crate::handles::SharedHandles;
use crate::policy::{PolicyEffect, PolicyEngine};
use crate::result::{ActionResult, ErrorKind};

// ── Always-compiled primitives (the "tronc commun") ──────────────────────────
pub mod artifact;
pub mod assert;
pub mod cloud;
pub mod confine;
pub mod container_group;
pub mod desktop;
pub mod efficient;
mod fs;
mod html_text;
pub mod human;
pub mod if_else;
pub mod interactive;
pub mod isolation;
pub mod limits;
pub mod listen;
pub mod listen_batch;
mod logs;
pub mod memory;
pub mod meta;
pub mod patch;
pub mod pipeline;
pub mod podman_bin;
pub mod process;
pub mod sandbox;
pub mod sandbox_child;
pub mod scripted;
mod search;
pub mod simulate;
pub mod snapshot;
pub mod ssh_exec;
pub mod system;
pub mod template;
pub mod term;
pub mod try_block;
pub mod validate;
pub mod watch_react;
mod web; // always compiled — RateLimiter is a runtime dependency of
         // process/sandbox actions, but the user-facing Action::Click
         // etc. are gated by `prim-desktop` below.

// ── Feature-gated families ───────────────────────────────────────────────────
// `prim-git` removed — `git` is a shell command, not a typed primitive.
#[cfg(feature = "prim-events")]
pub mod events;
#[cfg(feature = "prim-ml")]
pub mod ml;
// `prim-incus` was the gating feature for the in-runtime Incus executor.
// Incus actions now live in x0ne-orchestrator (privilege boundary fix);
// the runtime no longer carries any Incus code.

use desktop::RateLimiter;

/// Returns a `FeatureDisabled` error result for an action that was compiled
/// out of this build.  The variant is still matched (so the enum stays
/// exhaustive and the wire format stable), but execution is refused.
///
/// `#[allow(dead_code)]` because in a fully-featured build (every `prim-*`
/// enabled) no match arm references this helper.
#[allow(dead_code)]
fn feature_disabled(action_type: &str, feature: &str) -> ActionResult {
    ActionResult::err(
        action_type,
        ErrorKind::FeatureDisabled,
        format!(
            "Action '{}' is not compiled in this build. \
             Re-build with `--features {}`.",
            action_type, feature
        ),
        0,
    )
}

/// Feature-gated match arm for an action.
///
/// Usage:
///
/// ```ignore
/// gated_arm!("prim-git", "GitStatus", git::git_status(action, config).await)
/// ```
///
/// Expands to the executor expression when the feature is on, and to a
/// uniform `FeatureDisabled` result otherwise. Importantly, the disabled
/// arm does *not* reference the gated module, so omitting it from the build
/// does not cause "unresolved import" errors.
macro_rules! gated_arm {
    ($feature:literal, $name:literal, $expr:expr) => {{
        #[cfg(feature = $feature)]
        {
            $expr
        }
        #[cfg(not(feature = $feature))]
        {
            feature_disabled($name, $feature)
        }
    }};
}

/// Dispatch an already policy-approved [`Action`] to the correct executor.
///
/// `handles` is the shared handle manager for streaming and event actions.
/// `policy` is needed for `PolicyCheck` (meta action that re-evaluates).
/// `rate_limiter` throttles desktop input actions.
/// `composites` is the store for user-defined composite primitives.
pub async fn execute(
    action: &Action,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    let start = std::time::Instant::now();

    // SECURITY / BUG 2026-08-16: the executor enforces confinement a second
    // time, on `config.policy`, and that config is the file as loaded from
    // disk. A root the operator granted at a prompt lived only in the engine,
    // so a granted read was allowed by the gate and refused by the executor
    // 0.1 ms later. `config_with_session_grants` is the ONE place the two
    // views are reconciled, and it only adds roots the engine already
    // validated. `None` — the ordinary case — hands the executor the
    // operator's own config, untouched and uncloned.
    let widened = policy.config_with_session_grants(config);
    let config = widened.as_ref().unwrap_or(config);

    let result =
        execute_action_at_depth(action, config, handles, policy, rate_limiter, composites, 0).await;

    // Patch the duration into the result
    let duration_ms = start.elapsed().as_millis() as u64;
    ActionResult {
        duration_ms,
        ..result
    }
}

/// Inner dispatch without timing wrapper (also called from pipeline.rs).
///
/// `#[allow(unused_variables)]` because some `Action::*` variants are matched
/// with named fields whose use is gated behind a `prim-*` Cargo feature.
/// When that feature is disabled, the `gated_arm!` macro expands to
/// `feature_disabled(...)` which doesn't reference those fields, producing
/// spurious "unused variable" warnings. The fields are *used* in any
/// realistic build (default features) and the warning would mask real bugs.
#[allow(unused_variables)]
pub async fn execute_action(
    action: &Action,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    execute_action_at_depth(action, config, handles, policy, rate_limiter, composites, 0).await
}

/// Execute an Action **nested inside another Action**, with the same policy
/// semantics it would receive at the runtime's own entry point.
///
/// [`execute_action`] is deliberately unpoliced: policy is the caller's
/// obligation, and at the process boundary that obligation is discharged once,
/// in `server.rs` and `mcp/server.rs`. Nested execution has no such boundary,
/// so this function is it.
///
/// Without it, a check could synthesize an action and run it without the
/// gate — which is exactly what `Assert { check: "command_succeeds" }` did.
/// `Assert` is classed `ReadOnly` and is therefore enabled by default, so a
/// runtime configured `mode = "readonly"` with `RunCommand = false` and an
/// empty command allowlist would still spawn the process. The regression
/// tests in `assert.rs` prove it by side effect: the command's marker file
/// was created.
///
/// The rule this encodes:
///
/// > No model-reachable nested or synthesized Action may execute without the
/// > policy semantics that Action would receive at the normal entry point.
///
/// A denial is returned as the same `PolicyDenied` envelope the HTTP and MCP
/// entry points produce, so a caller cannot tell — and must not care — whether
/// the refusal came from the boundary or from here.
pub async fn execute_nested(
    action: &Action,
    config: &AppConfig,
    handles: SharedHandles,
    policy: &PolicyEngine,
    rate_limiter: &RateLimiter,
    composites: &CompositeStore,
) -> ActionResult {
    match policy.evaluate(action) {
        PolicyEffect::Allow => {}
        PolicyEffect::RequireConfirmation(msg) => {
            tracing::warn!(
                action_type = action.type_name(),
                "Nested action requires user approval — refusing"
            );
            return ActionResult::err(
                action.type_name(),
                ErrorKind::PolicyDenied,
                format!("Action requires user approval: {}", msg),
                0,
            );
        }
        PolicyEffect::Deny(reason) => {
            tracing::warn!(
                action_type = action.type_name(),
                reason = ?reason,
                "Nested action denied by policy"
            );
            return ActionResult::err(
                action.type_name(),
                ErrorKind::PolicyDenied,
                reason.message(),
                0,
            );
        }
    }
    execute_action(action, config, handles, policy, rate_limiter, composites).await
}

/// Inner dispatch with pipeline nesting depth tracking.
///
/// `depth` is 0 for top-level calls and incremented by pipeline.rs
/// when executing sub-steps. This enables nested pipelines up to
/// `MAX_PIPELINE_DEPTH` levels.
///
/// Returns a `BoxFuture` (not `async fn`) to make the future type
/// explicitly `Send`. This breaks the recursive type cycle created
/// by WatchAndReact → tokio::spawn → execute_action → WatchAndReact.
#[allow(unused_variables)]
pub fn execute_action_at_depth<'a>(
    action: &'a Action,
    config: &'a AppConfig,
    handles: SharedHandles,
    policy: &'a PolicyEngine,
    rate_limiter: &'a RateLimiter,
    composites: &'a CompositeStore,
    depth: usize,
) -> futures::future::BoxFuture<'a, ActionResult> {
    Box::pin(async move {
        match action {
            // ── Filesystem ───────────────────────────────────────────────────────
            Action::ReadFile { .. } => fs::read_file(action, config).await,
            Action::ReadFiles { .. } => efficient::read_files(action, config).await,
            Action::WriteFile { .. } => fs::write_file(action, config).await,
            Action::EditFile { .. } => fs::edit_file(action, config).await,
            Action::ApplyPatch { .. } => patch::apply_patch(action, config).await,
            Action::StatFiles { .. } => efficient::stat_files(action, config).await,
            Action::PatchSet { .. } => efficient::patch_set(action, config).await,
            Action::SearchAndRead { .. } => {
                efficient::search_and_read(action, config, policy).await
            }
            Action::SafeTest { .. } => efficient::safe_test(action, config).await,
            Action::MoveToTrash { .. } => fs::move_to_trash(action, config).await,

            // ── Search ──────────────────────────────────────────────────────────
            Action::SearchCode { .. } => search::search_code(action, config, policy).await,

            // ── Network (feature: prim-network) ─────────────────────────────────
            Action::FetchUrl {
                url,
                headers,
                format,
                timeout,
                offset,
            } => {
                gated_arm!(
                    "prim-network",
                    "FetchUrl",
                    system::fetch_url(
                        url,
                        headers.as_ref(),
                        &system::FetchOpts {
                            format: format.clone(),
                            timeout_secs: *timeout,
                            offset: *offset,
                        },
                    )
                    .await
                )
            }
            Action::HttpRequest {
                url,
                method,
                headers,
                body,
                timeout_secs,
            } => {
                gated_arm!(
                    "prim-network",
                    "HttpRequest",
                    system::http_request(url, method, headers, body.as_deref(), *timeout_secs)
                        .await
                )
            }
            Action::WebSearch { query, limit } => {
                gated_arm!(
                    "prim-network",
                    "WebSearch",
                    system::web_search(query, *limit).await
                )
            }

            // ── Process ─────────────────────────────────────────────────────────
            Action::RunCommand { .. } => {
                process::run_command(action, config, handles.clone()).await
            }
            Action::SshExec { .. } => ssh_exec::ssh_exec(action, config).await,
            Action::CloudCost { .. } => cloud::cloud_cost(action, config).await,
            Action::GpuStatus { .. } => cloud::gpu_status(action, config).await,
            Action::RunInSandbox { .. } => sandbox::run_in_sandbox(action, config).await,
            Action::RunContainerGroup { .. } => {
                container_group::run_container_group(action, config).await
            }
            Action::SendInput { .. } => interactive::send_input(action, handles.clone()).await,
            Action::Listen { .. } => listen::listen(action, handles.clone(), config).await,
            Action::TermOpen { .. }
            | Action::TermSend { .. }
            | Action::TermRead { .. }
            | Action::TermWait { .. }
            | Action::TermResize { .. }
            | Action::TermSnapshot { .. }
            | Action::TermList
            | Action::TermClose { .. } => term::execute(action, config, handles.clone()).await,
            Action::WatchAndReact { .. } => {
                watch_react::watch_and_react(
                    action,
                    config,
                    handles.clone(),
                    policy,
                    rate_limiter,
                    composites,
                )
                .await
            }

            // ── Incus / parallel containers — moved to x0ne-orchestrator ────────
            // RunIncus, RunIncusGroup, and IncusCreate/Restore/List/Delete
            // Snapshot all live in x0ne-orchestrator/src/incus_actions.rs now.
            // The runtime no longer recognises them; the orchestrator intercepts
            // before forwarding to the bridge. The privilege boundary is enforced
            // by code structure, not by policy check.

            // ── Logs ────────────────────────────────────────────────────────────
            Action::ReadLogs { .. } => logs::read_logs(action, config).await,

            // ── Human ───────────────────────────────────────────────────────────
            Action::AskHuman { .. } => human::ask_human(action).await,
            Action::Confirm { .. } => human::confirm(action).await,
            Action::AskMultipleChoice { .. } => human::ask_multiple_choice(action).await,

            // ── Template (one-shot tool-call substitution) ───────────────────────
            Action::RenderTemplate {
                template,
                max_depth,
                strict,
            } => {
                Box::pin(template::render_template(
                    template,
                    *max_depth,
                    *strict,
                    config,
                    handles,
                    policy,
                    rate_limiter,
                    composites,
                    depth,
                ))
                .await
            }

            // ── Pipeline ────────────────────────────────────────────────────────
            Action::Pipeline {
                steps,
                stop_on_error,
                snapshot_before,
                parallel,
            } => {
                // Use Box::pin to break the async recursion cycle between
                // execute_action → execute_pipeline → execute_action
                Box::pin(pipeline::execute_pipeline(
                    steps,
                    *stop_on_error,
                    *parallel,
                    snapshot_before,
                    config,
                    policy,
                    handles,
                    rate_limiter,
                    composites,
                    depth,
                ))
                .await
            }

            // ── Try / Assert ────────────────────────────────────────────────────
            Action::Try {
                actions,
                rollback_on_fail,
                label,
                keep_snapshot,
            } => {
                Box::pin(try_block::execute_try(
                    actions,
                    *rollback_on_fail,
                    label,
                    *keep_snapshot,
                    config,
                    policy,
                    handles,
                    rate_limiter,
                    composites,
                ))
                .await
            }
            Action::SafeChange {
                steps,
                tests,
                rollback_on_fail,
                label,
                keep_snapshot,
            } => {
                Box::pin(try_block::execute_safe_change(
                    steps,
                    tests,
                    *rollback_on_fail,
                    label,
                    *keep_snapshot,
                    config,
                    policy,
                    handles,
                    rate_limiter,
                    composites,
                ))
                .await
            }
            Action::Assert {
                check,
                path,
                value,
                host,
                port,
                command,
                args,
                name,
                actual,
                expected,
            } => {
                let params_val = if_else::build_check_params(
                    path, value, host, port, command, args, name, actual, expected,
                );
                Box::pin(assert::execute_assert(
                    check,
                    &params_val,
                    config,
                    handles,
                    policy,
                    rate_limiter,
                    composites,
                ))
                .await
            }

            // ── If / Conditional ────────────────────────────────────────────────
            Action::If {
                check,
                path,
                value,
                host,
                port,
                command,
                args,
                name,
                actual,
                expected,
                then,
                else_branch,
            } => {
                let params_val = if_else::build_check_params(
                    path, value, host, port, command, args, name, actual, expected,
                );
                Box::pin(if_else::execute_if(
                    check,
                    &params_val,
                    then,
                    else_branch,
                    config,
                    policy,
                    handles,
                    rate_limiter,
                    composites,
                    depth,
                ))
                .await
            }

            // ── Snapshots ───────────────────────────────────────────────────────
            Action::CreateSnapshot { name, paths } => {
                snapshot::action_create_snapshot(name, paths, config).await
            }
            Action::RestoreSnapshot { name } => {
                snapshot::action_restore_snapshot(name, config).await
            }
            Action::ListSnapshots => snapshot::action_list_snapshots(config).await,
            Action::DeleteSnapshot { name } => snapshot::action_delete_snapshot(name, config).await,

            // ── Subscribe / Handles (feature: prim-events) ──────────────────────
            Action::Subscribe { event_type, .. } => {
                gated_arm!(
                    "prim-events",
                    "Subscribe",
                    events::subscribe(event_type, config, handles.clone()).await
                )
            }
            Action::CheckHandle { handle } => {
                gated_arm!(
                    "prim-events",
                    "CheckHandle",
                    events::check_handle(handle, handles.clone()).await
                )
            }
            Action::StopHandle { handle } => {
                gated_arm!(
                    "prim-events",
                    "StopHandle",
                    events::stop_handle(handle, handles.clone()).await
                )
            }
            Action::ListHandles => {
                gated_arm!(
                    "prim-events",
                    "ListHandles",
                    events::list_handles(handles.clone()).await
                )
            }

            // ── Meta / Capability Discovery ──────────────────────────────────────
            Action::PolicyCheck { action: raw } => meta::policy_check(raw, policy, config),

            // Discovery. Derived from the vocabulary, so what a caller is told
            // it may emit is exactly what this dispatcher accepts.
            Action::ListActions => meta::list_actions(config, composites).await,
            Action::DescribeAction { name } => {
                meta::describe_action(name, config, composites).await
            }
            Action::Simulate { target, explain } => {
                simulate::simulate(target, *explain, policy, config)
            }

            // ── System (slim — see shell for trivia like uname/whoami/uptime) ───
            Action::FileInfo { path } => system::file_info(path, config).await,
            Action::Checksum { path, algorithm } => {
                system::checksum(path, algorithm.as_deref(), config).await
            }
            Action::ListProcesses { filter } => {
                system::list_processes(filter.as_deref(), config).await
            }
            Action::PortCheck {
                host,
                port,
                timeout_secs,
            } => {
                gated_arm!(
                    "prim-network",
                    "PortCheck",
                    system::port_check(host, *port, *timeout_secs).await
                )
            }
            Action::DnsLookup { hostname } => {
                gated_arm!(
                    "prim-network",
                    "DnsLookup",
                    system::dns_lookup(hostname).await
                )
            }

            // ── Memory ──────────────────────────────────────────────────────────
            Action::Remember {
                key,
                value,
                namespace,
            } => memory::remember(key, value, namespace.as_deref()).await,
            Action::Recall { key, namespace } => memory::recall(key, namespace.as_deref()).await,
            Action::Forget { key, namespace } => memory::forget(key, namespace.as_deref()).await,
            Action::ListMemory { namespace } => memory::list_memory(namespace.as_deref()).await,

            // ── Reflection & Validation ─────────────────────────────────────────
            Action::Diff {
                a,
                b,
                files,
                context_lines,
            } => validate::diff(a, b, *files, *context_lines, config).await,
            Action::Validate {
                value,
                check,
                expect,
            } => validate::validate(value, check, expect.as_ref()),

            // ── Crypto ───────────────────────────────────────────────────────────
            Action::RandomBytes { count, encoding } => {
                system::random_bytes(*count, encoding.as_deref()).await
            }

            // ── Desktop (feature: prim-desktop) ─────────────────────────────────
            Action::Screenshot { output, region } => {
                gated_arm!(
                    "prim-desktop",
                    "Screenshot",
                    desktop::screenshot(output.as_deref(), region.as_deref(), config).await
                )
            }
            Action::GetWindowTree => {
                gated_arm!(
                    "prim-desktop",
                    "GetWindowTree",
                    desktop::get_window_tree(config).await
                )
            }
            Action::GetFocusedWindow => {
                gated_arm!(
                    "prim-desktop",
                    "GetFocusedWindow",
                    desktop::get_focused_window(config).await
                )
            }
            Action::MoveMouse { x, y } => {
                gated_arm!(
                    "prim-desktop",
                    "MoveMouse",
                    desktop::move_mouse(*x, *y, config, rate_limiter).await
                )
            }
            Action::Click { x, y, button } => {
                gated_arm!(
                    "prim-desktop",
                    "Click",
                    desktop::click(*x, *y, button.as_deref(), config, rate_limiter).await
                )
            }
            Action::DoubleClick { x, y } => {
                gated_arm!(
                    "prim-desktop",
                    "DoubleClick",
                    desktop::double_click(*x, *y, config, rate_limiter).await
                )
            }
            Action::Scroll { direction, amount } => {
                gated_arm!(
                    "prim-desktop",
                    "Scroll",
                    desktop::scroll(direction, *amount, config, rate_limiter).await
                )
            }
            Action::TypeText { text } => {
                gated_arm!(
                    "prim-desktop",
                    "TypeText",
                    desktop::type_text(text, config, rate_limiter).await
                )
            }
            Action::PressKey { key } => {
                gated_arm!(
                    "prim-desktop",
                    "PressKey",
                    desktop::press_key(key, config, rate_limiter).await
                )
            }
            Action::PressKeyCombination { keys } => {
                gated_arm!(
                    "prim-desktop",
                    "PressKeyCombination",
                    desktop::press_key_combination(keys, config, rate_limiter).await
                )
            }
            Action::FocusWindow { match_str } => {
                gated_arm!(
                    "prim-desktop",
                    "FocusWindow",
                    desktop::focus_window(match_str, config).await
                )
            }
            Action::SendNotification {
                title,
                body,
                urgency,
            } => {
                gated_arm!(
                    "prim-desktop",
                    "SendNotification",
                    desktop::send_notification(title, body, urgency.as_deref()).await
                )
            }
            Action::GetClipboard => {
                gated_arm!(
                    "prim-desktop",
                    "GetClipboard",
                    desktop::get_clipboard(config).await
                )
            }
            Action::SetClipboard { content } => {
                gated_arm!(
                    "prim-desktop",
                    "SetClipboard",
                    desktop::set_clipboard(content, config).await
                )
            }

            // ── ML / LLM (feature: prim-ml) ─────────────────────────────────────
            Action::LlmCall {
                endpoint,
                model,
                prompt,
                system,
                temperature,
                max_tokens,
                auth,
            } => {
                gated_arm!(
                    "prim-ml",
                    "LlmCall",
                    ml::llm_call(
                        endpoint,
                        model,
                        prompt,
                        system.as_deref(),
                        *temperature,
                        *max_tokens,
                        auth.as_deref(),
                        config
                    )
                    .await
                )
            }
            Action::OllamaApi {
                endpoint,
                operation,
                model,
                modelfile,
                auth,
            } => {
                gated_arm!(
                    "prim-ml",
                    "OllamaApi",
                    ml::ollama_api(
                        endpoint,
                        operation,
                        model.as_deref(),
                        modelfile.as_deref(),
                        auth.as_deref(),
                        config
                    )
                    .await
                )
            }
            Action::DatasetGenerate {
                endpoint,
                model,
                topic,
                count,
                output,
                style,
            } => {
                gated_arm!(
                    "prim-ml",
                    "DatasetGenerate",
                    ml::dataset_generate(
                        endpoint,
                        model,
                        topic,
                        *count,
                        output,
                        style.as_deref(),
                        config
                    )
                    .await
                )
            }
            Action::DocumentExtract {
                input,
                output,
                format,
                llm_endpoint,
                llm_model,
                chunk_size,
            } => {
                gated_arm!(
                    "prim-ml",
                    "DocumentExtract",
                    ml::document_extract(
                        input,
                        output,
                        format,
                        llm_endpoint.as_deref(),
                        llm_model.as_deref(),
                        *chunk_size,
                        config
                    )
                    .await
                )
            }
            Action::OllamaChat {
                endpoint,
                model,
                messages,
                temperature,
                max_tokens,
                auth,
            } => {
                gated_arm!(
                    "prim-ml",
                    "OllamaChat",
                    ml::ollama_chat(
                        endpoint,
                        model,
                        messages,
                        *temperature,
                        *max_tokens,
                        auth.as_deref(),
                        config
                    )
                    .await
                )
            }

            // ── Composite Primitives ─────────────────────────────────────────────
            Action::DefinePrimitive { .. } => {
                crate::composite::handle_define(action, composites, config).await
            }
            Action::CallPrimitive { .. } => {
                Box::pin(crate::composite::handle_call(
                    action,
                    composites,
                    config,
                    policy,
                    handles,
                    rate_limiter,
                    0,
                ))
                .await
            }
            Action::ListCustomPrimitives => crate::composite::handle_list(composites).await,
            Action::DeletePrimitive { .. } => {
                crate::composite::handle_delete(action, composites).await
            }

            // ── Artifact (web artifacts served by x0ne-artifact-proxy) ──────────────
            Action::ArtifactCreate { .. } => artifact::artifact_create(action, config).await,
            Action::ArtifactList => artifact::artifact_list(action, config).await,
            Action::ArtifactServe { .. } => artifact::artifact_serve(action, config).await,
            Action::ArtifactSave { .. } => artifact::artifact_save(action, config).await,
            Action::ArtifactStop { .. } => artifact::artifact_stop(action, config).await,
            Action::ArtifactDestroy { .. } => artifact::artifact_destroy(action, config).await,

            // ── Orchestrator-owned UI passthrough ──────────────────────────────
            //
            // PaintBadge is a meta-action: the runtime doesn't paint
            // anything itself, but it accepts the variant so the
            // orchestrator can embed it inside Pipeline/If/Try steps
            // without the runtime rejecting an unknown variant. The
            // result echoes the payload back via output.kind so the
            // orchestrator can scan Pipeline results and route each
            // PaintBadge to its BadgeUpdate event.
            Action::PaintBadge { payload } => ActionResult::ok(
                "PaintBadge",
                crate::result::ActionOutput::Generic {
                    data: serde_json::json!({
                        "kind": "badge_paint",
                        "payload": payload,
                    }),
                },
                0,
            ),

            // ── Scripted Functions (user-defined Rhai primitives — Tier 1) ──────
            Action::DefineFunction { .. } => scripted::define_function(action, config).await,
            Action::CallFunction { .. } => scripted::call_function(action, config).await,
            Action::ListFunctions => scripted::list_functions(action, config).await,
            Action::DeleteFunction { .. } => scripted::delete_function(action, config).await,

            // ── asm-ai (Tier 2 capability) ───────────────────────────────────
            // `RunProgram` is unreachable in practice as long as the policy
            // refuses: the FsWrite class sets `RunProgram = false` in
            // `config::defaults()`. `PreviewProgram`, however, IS REACHABLE —
            // its ReadOnly class enables it by default — so it needs a clean
            // answer. In both cases a `todo!()` would be a panic in an
            // executor, and an executor that panics kills the session instead
            // of returning an error to the model.
            //
            // `ErrorKind::NotImplemented` (result.rs:1346) is produced nowhere
            // else in this crate: the usual pattern is `feature_disabled()`
            // (:59-70), which speaks of a cargo feature. Here it is not a
            // feature, it is a plan step.
            //
            // The real body arrives with the asm-ai plan: compilation,
            // verification and preview in T3, host in T5-T6, commit in T8. The
            // two tests I-A (frozen import signature table) and « pure module »
            // arrive with the ABI, in T3 — a deliberate deviation from the spec,
            // which files them under task 1; here they have no crate or module
            // to bite on.
            // T3: the real body. It compiles, checks every expanded path
            // against the policy, and returns the proven list. Nothing is
            // executed (T6) and nothing is written (T8).
            Action::PreviewProgram { source } => crate::asm_verify::preview(source, policy, config),
            Action::RunProgram { .. } => ActionResult::err(
                "RunProgram",
                ErrorKind::NotImplemented,
                "asm-ai programs are not executable in this build yet",
                0,
            ),
        }
    }) // Box::pin(async move)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod feature_tests {
    use super::*;
    use crate::result::ErrorKind;

    /// `feature_disabled()` produces a well-formed error with the expected
    /// `ErrorKind` and a message that names the action and the feature flag
    /// the user must enable. Always present (helper is `#[allow(dead_code)]`
    /// when nothing calls it but the test always exercises it).
    #[test]
    fn feature_disabled_helper_shape() {
        let r = feature_disabled("MyAction", "prim-foo");
        assert!(!r.success);
        assert_eq!(r.action_type, "MyAction");
        let err = r.error.expect("expected an error payload");
        assert!(matches!(err.kind, ErrorKind::FeatureDisabled));
        assert!(err.message.contains("MyAction"), "message: {}", err.message);
        assert!(err.message.contains("prim-foo"), "message: {}", err.message);
    }

    // Git-family feature-gating tests removed alongside GitStatus / GitDiff.
}
