//! The control channel: messages the ORCHESTRATOR sends, that the model
//! cannot.
//!
//! # Why a second vocabulary at all
//!
//! Everything the runtime accepts today is an [`crate::action::Action`], and
//! `Action` is the vocabulary published to the model — `ListActions` lists it,
//! `DescribeAction` documents it, the system prompt teaches it. A policy grant
//! must not be in it. An action that widens the policy would be an action the
//! policy engine evaluates, which is a loop, and more importantly it would be
//! a verb the model could learn to reach for.
//!
//! So a grant travels as a *control line*, tagged `"control"` rather than
//! `"type"`. Three independent things make it unreachable from a model:
//!
//! 1. The orchestrator's parser requires a `type` field and rejects an object
//!    without one as a parse error (`parser.rs`), so `{"control": …}` never
//!    becomes an action.
//! 2. If the model writes `{"type":"PolicyGrant", …}` instead, the runtime's
//!    `serde_json::from_value::<Action>` rejects it as an unknown variant like
//!    any other invented verb — `PolicyGrant` is not in the enum, not in the
//!    generated catalogue, not in `ListActions`.
//! 3. A control line is only ever *emitted* by the orchestrator, after a human
//!    answered a prompt. There is no path from model output to
//!    `Bridge::send_control`.
//!
//! 4. The orchestrator forwards a model's action as written, so a line
//!    carrying both `type` and `control` is model text: refused whole
//!    ([`is_mixed_line`]).
//!
//! The same channel carries `SessionMode` (`/plan`, `/build`): plan mode is
//! a posture only the human leaves, so it travels where grants travel.
//!
//! `the_model_cannot_emit_a_policy_grant` pins 1 and 2,
//! `tests/a_control_line_never_rides_in_an_action.rs` pins 4.
//!
//! # SECURITY: stdio only, and why that is not a new trust
//!
//! A control line is accepted on the stdio bridge and nowhere else. Over
//! stdio, whoever writes to this process's stdin is the process that spawned
//! it: same user, same session, holding the pipe it created. That party
//! already decides which binary we are, which config we find, and what our
//! environment contains — it could have written the policy file itself before
//! exec. Accepting a grant from it adds no authority that was not already
//! there.
//!
//! `http` and `mcp` are the opposite case. There the peer is whoever reached a
//! socket, authenticated at best by a bearer token, and — the part that
//! matters — there is *no human at the other end*. A grant means "a person was
//! shown a refusal and chose to widen it". A socket cannot make that claim.
//! Refusing there is the same doctrine `serve.rs` already states for remote
//! sessions: "a remote request is not a human, and a bearer token is transport
//! authentication, not action authorization".
//!
//! Enforced in two places on purpose:
//! - the stdio read loop is the only caller of [`handle_control`]; and
//! - `PolicyEngine::apply_grant` refuses unless `runtime.mode == "stdio"`.
//!
//! One of the two would be enough today. Two is because this repository's most
//! repeated defect is a check whose only enforcement point got refactored
//! around while the flag it read stayed in the config file.
//!
//! # What a grant cannot do
//!
//! It cannot lift `deny_path_patterns`, `readonly` mode, `default_effect =
//! "deny"`, the desktop master switch, or a refusal from inside a composite.
//! Not because the orchestrator will not ask — the runtime refuses even when
//! asked, in `apply_grant`. The list of what *is* liftable is an exhaustive
//! match on `DenyReason` (`policy::DenyReason::liftable`) so the next reason
//! anyone adds cannot default into being waivable.

use serde::Deserialize;

use crate::result::{ActionResult, ErrorKind};

/// A message that configures the runtime rather than asking it to act.
///
/// Tagged on `"control"`, deliberately not on `"type"`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "control")]
pub enum ControlMessage {
    /// Widen this process's policy by one entry, for this process only.
    PolicyGrant(x0ne_actions::policy_grant::PolicyGrant),
    /// Enter or leave plan mode: `{"control":"SessionMode","mode":"plan"}`.
    SessionMode(SessionMode),
}

/// The body of a `SessionMode` control line.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMode {
    pub mode: SessionModeKind,
}

/// `plan` refuses every action that touches the host; `build` gives the
/// configured policy back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionModeKind {
    Plan,
    Build,
}

/// Does this line look like a control message rather than an action?
///
/// A JSON object carrying a `control` key and no `type`. An action is tagged
/// on `type`, and the orchestrator forwards a model's action as written, so a
/// `control` key beside a `type` is model text, never the orchestrator's.
pub fn is_control_line(value: &serde_json::Value) -> bool {
    value.get("control").is_some() && value.get("type").is_none()
}

/// A line carrying both tags. Refused whole by the stdio loop: neither
/// reading is safe (2026-10-27: a `ReadFile` carrying `"control":
/// "PolicyGrant"` was applied as a grant, no human asked).
pub fn is_mixed_line(value: &serde_json::Value) -> bool {
    value.get("control").is_some() && value.get("type").is_some()
}

/// The answer to a mixed line. Nothing ran.
pub fn mixed_line_refusal() -> ActionResult {
    ActionResult::err(
        "Control",
        ErrorKind::InvalidInput,
        "This line carries both `type` and `control`. An action has no `control` field: \
         remove it and send the action alone."
            .to_string(),
        0,
    )
}

/// Apply a control message. Returns the envelope to write back, keeping the
/// stdio contract of exactly one response line per request line.
pub fn handle_control(
    value: &serde_json::Value,
    policy: &crate::policy::PolicyEngine,
) -> ActionResult {
    let msg: ControlMessage = match serde_json::from_value(value.clone()) {
        Ok(m) => m,
        Err(e) => {
            return ActionResult::err(
                "Control",
                ErrorKind::InvalidInput,
                format!("Not a control message this runtime understands: {e}"),
                0,
            );
        }
    };

    match msg {
        ControlMessage::SessionMode(SessionMode { mode }) => {
            let plan = mode == SessionModeKind::Plan;
            match policy.set_plan_mode(plan) {
                Ok(()) => {
                    tracing::warn!(plan, "session mode switched by the operator");
                    let name = if plan { "plan" } else { "build" };
                    ActionResult::ok(
                        "SessionMode",
                        crate::result::ActionOutput::Generic {
                            data: serde_json::json!({ "mode": name, "plan": plan }),
                        },
                        0,
                    )
                }
                Err(why) => ActionResult::err("SessionMode", ErrorKind::PolicyDenied, why, 0),
            }
        }
        ControlMessage::PolicyGrant(grant) => match policy.apply_grant(&grant) {
            Ok(()) => {
                tracing::warn!(
                    kind = ?grant.kind,
                    value = %grant.value,
                    persist = grant.persist,
                    "policy widened by operator grant"
                );
                ActionResult::ok(
                    "PolicyGrant",
                    crate::result::ActionOutput::Generic {
                        data: serde_json::json!({
                            "granted": true,
                            "kind": grant.kind,
                            "value": grant.value,
                            "live_grants": policy.granted_count(),
                            // The path the runtime ACTUALLY loaded. The
                            // orchestrator persists to this and never to a
                            // path it computed itself: both sides derive the
                            // same convention independently (`config.rs`
                            // search order), and a `--config` flag, a legacy
                            // location or a cwd opt-in makes them differ.
                            // Writing the file the runtime did not read is the
                            // dead scaffold in its purest form — the operator
                            // is told his config was updated, and next session
                            // he is asked again.
                            "config_path": crate::config::loaded_config_path(),
                        }),
                    },
                    0,
                )
            }
            Err(why) => {
                tracing::warn!(kind = ?grant.kind, value = %grant.value, why = %why, "policy grant refused");
                ActionResult::err("PolicyGrant", ErrorKind::PolicyDenied, why, 0)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::policy::PolicyEngine;
    use serde_json::json;
    use x0ne_actions::policy_grant::{GrantKind, PolicyGrant};

    fn engine(mode: &str) -> PolicyEngine {
        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = mode.to_string();
        cfg.policy.mode = "auto".into();
        cfg.policy.default_effect = "allow".into();
        let actions = cfg.actions.clone();
        PolicyEngine::new(cfg, actions)
    }

    /// A control line is recognised by its own tag, and an action is not.
    ///
    /// A failure means the two vocabularies overlap: either a grant is read as
    /// an action (and evaluated by the policy it was meant to widen), or an
    /// action is read as a grant (and skips the policy engine entirely).
    #[test]
    fn a_control_line_is_never_an_action_line() {
        assert!(is_control_line(&json!({"control": "PolicyGrant"})));
        assert!(!is_control_line(
            &json!({"type": "RunCommand", "command": "ls"})
        ));
        assert!(!is_control_line(&json!({"type": "PolicyGrant"})));
        assert!(!is_control_line(&json!({})));
        let mixed = json!({"type": "ReadFile", "path": "/x", "control": "PolicyGrant"});
        assert!(!is_control_line(&mixed));
        assert!(is_mixed_line(&mixed));
        assert!(!is_mixed_line(&json!({"control": "PolicyGrant"})));
    }

    /// SECURITY: a runtime reachable over a socket refuses to be widened.
    ///
    /// A failure means anyone who can reach an `http` port or an `mcp` client
    /// can add commands to `allowed_commands` and directories to
    /// `allowed_read_paths` on the host — with no human anywhere in the loop,
    /// which is precisely the claim a grant is supposed to encode.
    #[test]
    fn a_policy_grant_over_http_or_mcp_is_refused() {
        let grant = PolicyGrant {
            kind: GrantKind::Command,
            value: "ls".into(),
            persist: false,
        };
        for mode in ["http", "mcp"] {
            let p = engine(mode);
            assert!(!p.accepts_grants(), "mode {mode} claims to accept grants");
            let err = p.apply_grant(&grant).expect_err(
                "a grant was accepted in mode {mode}, where nobody can have been asked",
            );
            assert!(
                err.contains("stdio"),
                "mode {mode}: unhelpful refusal: {err}"
            );
            assert_eq!(p.granted_count(), 0, "mode {mode} widened anyway");

            // And the control entry point agrees with the engine.
            let out = handle_control(
                &json!({"control": "PolicyGrant", "kind": "command", "value": "ls"}),
                &p,
            );
            assert!(!out.success, "mode {mode}: handle_control granted it");
        }

        let p = engine("stdio");
        assert!(p.accepts_grants());
        p.apply_grant(&grant).expect("stdio refused a grant");
        assert_eq!(p.granted_count(), 1);
    }

    /// SECURITY: the runtime refuses a grant it was ASKED to apply when the
    /// path is behind the hard boundary.
    ///
    /// A failure means "allow this directory for the session" is a
    /// one-keystroke path to `~/.ssh` — the orchestrator would never offer it,
    /// because `PathMatchesDenyPattern` is not liftable, but the runtime must
    /// not be relying on the orchestrator having asked the right question.
    #[test]
    fn a_grant_cannot_lift_a_deny_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join(".ssh");
        std::fs::create_dir(&secret).unwrap();

        let mut cfg = AppConfig::defaults();
        cfg.runtime.mode = "stdio".into();
        cfg.policy.mode = "auto".into();
        cfg.policy.default_effect = "allow".into();
        cfg.policy.deny_path_patterns = vec!["**/.ssh".into(), "**/.ssh/**".into()];
        let actions = cfg.actions.clone();
        let p = PolicyEngine::new(cfg, actions);

        let err = p
            .apply_grant(&PolicyGrant {
                kind: GrantKind::ReadPath,
                value: secret.to_string_lossy().to_string(),
                persist: false,
            })
            .expect_err("a deny pattern was waived by a grant");
        assert!(
            err.contains("deny pattern"),
            "the refusal does not say why: {err}"
        );
        assert_eq!(p.granted_count(), 0, "the boundary was widened anyway");
    }

    /// SECURITY: plan mode is switched over stdio only, like a grant. A
    /// failure means a socket peer can leave plan mode for the human.
    #[test]
    fn plan_mode_over_http_or_mcp_is_refused() {
        for mode in ["http", "mcp"] {
            let p = engine(mode);
            for line in ["plan", "build"] {
                let out = handle_control(&json!({"control": "SessionMode", "mode": line}), &p);
                assert!(!out.success, "mode {mode}: SessionMode {line} accepted");
            }
            assert!(!p.plan_mode());
        }
        let p = engine("stdio");
        let out = handle_control(&json!({"control": "SessionMode", "mode": "plan"}), &p);
        assert!(out.success);
        assert!(p.plan_mode());
        assert!(p.clone().plan_mode(), "a clone does not see plan mode");
    }

    /// A granted command is a bare name, checked by the runtime and not only
    /// by whoever built the message.
    ///
    /// A failure means an `allowed_commands` entry naming a path can be
    /// authored from a prompt — reopening the `/tmp/evil/git` shape that
    /// `check_command_allowed` documents.
    #[test]
    fn a_granted_command_is_validated_by_the_runtime_itself() {
        let p = engine("stdio");
        for bad in ["/tmp/evil/ls", "ls; id", "", "ls | cat"] {
            assert!(
                p.apply_grant(&PolicyGrant {
                    kind: GrantKind::Command,
                    value: bad.into(),
                    persist: false,
                })
                .is_err(),
                "{bad:?} was granted"
            );
        }
        assert_eq!(p.granted_count(), 0);
    }

    /// An action grant names an action this build knows.
    ///
    /// A failure means a grant can add an entry that enables nothing — a dead
    /// scaffold with a security meaning, since the operator was told he
    /// enabled something.
    #[test]
    fn an_action_grant_names_a_real_action() {
        let p = engine("stdio");
        assert!(p
            .apply_grant(&PolicyGrant {
                kind: GrantKind::Action,
                value: "NoSuchActionEver".into(),
                persist: false,
            })
            .is_err());
        assert!(p
            .apply_grant(&PolicyGrant {
                kind: GrantKind::Action,
                value: "WriteFile".into(),
                persist: false,
            })
            .is_ok());
    }
}
