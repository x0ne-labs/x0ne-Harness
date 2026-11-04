//! The actions tui handles itself, typed.
//!
//! `runner::dispatch_action` intercepts a handful of actions before the bridge
//! call — badge painting, the AskUser modal, the task panel, cron-light. They
//! are frontend concerns with no runtime meaning, so they are deliberately NOT
//! `x0ne_actions::Action` variants: the trusted core's vocabulary should not
//! grow to describe a TUI.
//!
//! What they DO share is the core's *semantics*. Each one declares a
//! [`SideEffectClass`] from `x0ne-actions`, using the same meanings the runtime
//! policy engine uses, and [`Intercepted::side_effect_class`] matches
//! exhaustively with no wildcard arm — so a new intercept cannot be added
//! without deciding, at compile time, what it does to the host.
//!
//! This matters because an intercepted action never reaches the runtime. Every
//! guarantee the policy engine would have provided has to be provided here
//! instead, and until this module existed the dispatcher was a run of
//! `if action_type == "…"` string comparisons with no semantics attached at
//! all. That is how `BadgeWatch` came to hand a model-supplied command to
//! `bash -c` with no policy check on a surface reachable from remote chat.

use x0ne_actions::SideEffectClass;

/// An action tui executes itself instead of forwarding to the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intercepted {
    /// Paint a frame into the badge canvas.
    PaintBadge,
    /// Open a modal question and block the turn on the human's answer.
    AskUser,
    /// Arm a recurring command whose stdout is painted into the badge.
    BadgeWatch,
    /// Disarm the badge watcher.
    BadgeWatchClear,
    /// Append a row to the Tasks panel.
    TaskAdd,
    /// Mark a Tasks row done.
    TaskDone,
    /// Empty the Tasks panel.
    TaskClear,
    /// Defer a prompt, a reminder, or an arbitrary action to a due time.
    CronSchedule,
    /// Cancel a deferred entry.
    CronCancel,
}

impl Intercepted {
    /// Every intercepted action. The anchoring tests walk this, so a variant
    /// added to the enum but forgotten here is caught rather than silently
    /// escaping the invariants below.
    pub const ALL: [Intercepted; 9] = [
        Intercepted::PaintBadge,
        Intercepted::AskUser,
        Intercepted::BadgeWatch,
        Intercepted::BadgeWatchClear,
        Intercepted::TaskAdd,
        Intercepted::TaskDone,
        Intercepted::TaskClear,
        Intercepted::CronSchedule,
        Intercepted::CronCancel,
    ];

    /// The single place a wire `type` string becomes an intercept decision.
    pub fn from_type_name(name: &str) -> Option<Self> {
        Some(match name {
            "PaintBadge" => Intercepted::PaintBadge,
            "AskUser" => Intercepted::AskUser,
            "BadgeWatch" => Intercepted::BadgeWatch,
            "BadgeWatchClear" => Intercepted::BadgeWatchClear,
            "TaskAdd" => Intercepted::TaskAdd,
            "TaskDone" => Intercepted::TaskDone,
            "TaskClear" => Intercepted::TaskClear,
            "CronSchedule" => Intercepted::CronSchedule,
            "CronCancel" => Intercepted::CronCancel,
            _ => return None,
        })
    }

    pub fn type_name(self) -> &'static str {
        match self {
            Intercepted::PaintBadge => "PaintBadge",
            Intercepted::AskUser => "AskUser",
            Intercepted::BadgeWatch => "BadgeWatch",
            Intercepted::BadgeWatchClear => "BadgeWatchClear",
            Intercepted::TaskAdd => "TaskAdd",
            Intercepted::TaskDone => "TaskDone",
            Intercepted::TaskClear => "TaskClear",
            Intercepted::CronSchedule => "CronSchedule",
            Intercepted::CronCancel => "CronCancel",
        }
    }

    /// What this action does to the host, in the shared vocabulary's terms.
    ///
    /// No wildcard arm, deliberately — mirroring
    /// `x0ne_actions::Action::side_effect_class`. Adding an intercept forces
    /// this decision, and anything that is not `Meta` must be justified
    /// against the runtime guarantees it bypasses.
    pub fn side_effect_class(self) -> SideEffectClass {
        match self {
            // Runs a model-supplied command via `bash -c` on a timer. This is
            // the one intercept with a real host effect, which is why it is
            // submitted to the runtime policy engine before being armed.
            Intercepted::BadgeWatch => SideEffectClass::ProcessSpawn,
            // Carries an arbitrary nested action to run when due. It executes
            // nothing itself; the deferred action is re-dispatched through
            // `dispatch_action`, so it meets these same checks at fire time.
            Intercepted::CronSchedule => SideEffectClass::Composite,
            // Pure UI state: badge pixels, a modal, the task panel, and
            // cancelling either of the two armed things above.
            Intercepted::PaintBadge
            | Intercepted::AskUser
            | Intercepted::BadgeWatchClear
            | Intercepted::TaskAdd
            | Intercepted::TaskDone
            | Intercepted::TaskClear
            | Intercepted::CronCancel => SideEffectClass::Meta,
        }
    }

    /// Whether arming this action requires the runtime policy engine's consent.
    ///
    /// True for exactly the classes that touch the host. `Composite` is false
    /// here because the nested action is checked when it actually fires, not
    /// when it is scheduled.
    pub fn requires_policy_authorization(self) -> bool {
        // SECURITY: the shared definition, not a local copy of it. This list
        // was written out here, again in `agent/dispatch.rs` and a third time
        // in `terminal.rs` — and the third one left `Network` out, which is
        // how `SshExec` reached a remote shell with no human asked.
        self.side_effect_class().touches_host()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ALL` is what the other anchoring tests iterate, so it has to actually
    /// be complete. Every name resolves back to its own variant.
    #[test]
    fn all_variants_round_trip_through_their_name() {
        for variant in Intercepted::ALL {
            assert_eq!(
                Intercepted::from_type_name(variant.type_name()),
                Some(variant),
                "{} does not round-trip",
                variant.type_name()
            );
        }
    }

    /// An intercepted action never reaches the runtime, so if a frontend
    /// intercept ever took the name of a real runtime action it would swallow
    /// it — the policy engine would never see it and the user would never be
    /// asked. `PaintBadge` is the one deliberate overlap: the runtime carries
    /// it as a `Meta`-classed stub purely so it can sit inside a Pipeline
    /// step. That is safe precisely because it is inert, and this test pins
    /// that condition rather than the exception.
    #[test]
    fn no_intercept_shadows_an_effectful_runtime_action() {
        for variant in Intercepted::ALL {
            let probe = serde_json::json!({"type": variant.type_name()});
            if let Ok(runtime) = serde_json::from_value::<x0ne_actions::Action>(probe) {
                assert_eq!(
                    runtime.side_effect_class(),
                    SideEffectClass::Meta,
                    "{} shadows a runtime action that has real effects — intercepting \
                     it here would bypass the policy engine entirely",
                    variant.type_name()
                );
            }
        }
    }

    /// The two dispatchers must not both claim a name. If they did, which one
    /// ran would depend on which frontend the user happened to be in — and
    /// `agent_loop`'s confirmation gate does not exist in tui.
    #[test]
    fn intercepts_are_disjoint_from_the_agent_loop_dispatch_table() {
        for variant in Intercepted::ALL {
            assert!(
                !crate::agent_loop::ORCHESTRATOR_ACTION_TYPES.contains(&variant.type_name()),
                "{} is claimed by both dispatch tables",
                variant.type_name()
            );
        }
    }

    /// The invariant that matters: exactly one intercept touches the host, and
    /// it is the one wired to the policy engine. A new effectful intercept
    /// fails here until it is authorized too.
    #[test]
    fn only_badge_watch_requires_policy_authorization() {
        let effectful: Vec<&str> = Intercepted::ALL
            .into_iter()
            .filter(|i| i.requires_policy_authorization())
            .map(|i| i.type_name())
            .collect();
        assert_eq!(
            effectful,
            vec!["BadgeWatch"],
            "an intercept with host effects must be submitted to the policy \
             engine before it runs — see the BadgeWatch arming path in runner.rs"
        );
    }
}
