//! One canonical budget for a turn.
//!
//! Every frontend had its own idea of when to stop. The CLI carried four
//! counters, TUI a single per-turn action cap plus a duplicate-action
//! fingerprint, Matrix only a frame-retry count, and serve a pair of action
//! caps — plus a monetary limit it logged at startup (*"sessions will stop at
//! $X"*) and then never read again. An operator reading that line held a false
//! belief about a spend ceiling that did not exist.
//!
//! So the limits live here, once, and a frontend may display them but cannot
//! enforce a different set.
//!
//! This module answers only "how much has this turn consumed?". Whether the
//! model is *repeating itself* is a different question, about behaviour rather
//! than quantity, and `agent::loop_detect` owns it. A `SameActionFailures`
//! counter keyed on the action name briefly lived here and was removed: it
//! asked the loop-detection question badly, since `WriteFile` failing on six
//! different paths is progress and only identity-plus-arguments can tell that
//! apart from a genuine loop.

/// Consecutive actions before the agent checks in with the user, when the
/// configuration does not say (`cost.max_consecutive_actions`; 0 = never).
/// The compiled 20 stopped a playground run every 20 actions (2026-08-17).
pub const MAX_CONSECUTIVE_ACTIONS: usize = 100;
/// Retries when the model emits an action block that will not parse.
pub const MAX_PARSE_RETRIES: usize = 5;
/// Absolute failure ceiling for one turn.
pub const MAX_TOTAL_FAILURES: usize = 16;

/// Why the engine stopped iterating.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetStop {
    /// Enough consecutive actions; hand control back to the user.
    ConsecutiveActions,
    /// The model could not produce a parseable action.
    ParseRetries,
    /// Too many failures of any kind this turn.
    TotalFailures,
    /// The session's monetary ceiling was reached.
    CostExhausted { spent_usd: f64, limit_usd: f64 },
}

impl BudgetStop {
    /// What the user is told. Each says which limit bound and what to do,
    /// because "stopped" on its own reads as a crash.
    pub fn message(&self) -> String {
        match self {
            BudgetStop::ConsecutiveActions => "Paused after the configured run of consecutive \
                 actions (cost.max_consecutive_actions) — say `continue` to keep going, or set \
                 it to 0 to never pause."
                .to_string(),
            BudgetStop::ParseRetries => format!(
                "The model could not produce a valid action in {MAX_PARSE_RETRIES} attempts. \
                 Returning to your input."
            ),
            BudgetStop::TotalFailures => format!(
                "{MAX_TOTAL_FAILURES} failures in this turn — stopping so the session does \
                 not spin."
            ),
            BudgetStop::CostExhausted {
                spent_usd,
                limit_usd,
            } => format!(
                "Session budget reached: ${spent_usd:.2} of ${limit_usd:.2}. Raise it with \
                 `/budget` or start a new session."
            ),
        }
    }
}

/// Counters for one user turn.
///
/// `spent_usd` accumulates across the session rather than the turn, because
/// that is what a monetary ceiling means to the person paying.
/// The live monetary bound, as a capability.
///
/// Narrow on purpose. It is not an authority: it cannot approve an action,
/// dispatch one, reach a bridge, touch history or edit configuration. It
/// controls one number that the driver already consults.
pub trait BudgetControl: Send {
    fn current_limit_usd(&self) -> f64;
    fn spent_usd(&self) -> f64;
    /// `Err` with a reason the model can act on. Never touches `spent_usd`:
    /// what was spent was spent, and a limit that could rewrite it would not
    /// be a limit.
    fn set_limit_usd(&mut self, limit: f64) -> Result<f64, String>;
    /// Money spent on this budget's behalf by someone else: a child agent's
    /// model calls (H2-1, audit of 2026-10-30). One session, one bill,
    /// whoever ran up the item.
    fn record_spend(&mut self, usd: f64);
}

#[derive(Debug, Clone)]
pub struct TurnBudget {
    consecutive_actions: usize,
    /// `cost.max_consecutive_actions`; 0 disables the pause.
    pub max_consecutive_actions: usize,
    parse_retries: usize,
    total_failures: usize,
    /// Session spend so far. Zero disables the monetary check.
    pub spent_usd: f64,
    /// Session ceiling. Zero means "no limit", matching the config default.
    pub limit_usd: f64,
    /// `cost.max_session_usd`: the highest `limit_usd` may go. Zero = none.
    pub ceiling_usd: f64,
}

impl BudgetControl for TurnBudget {
    fn current_limit_usd(&self) -> f64 {
        self.limit_usd
    }

    fn spent_usd(&self) -> f64 {
        self.spent_usd
    }

    fn set_limit_usd(&mut self, limit: f64) -> Result<f64, String> {
        if limit < 0.0 {
            return Err("Budget set requires a non-negative 'limit'".into());
        }
        let c = self.ceiling_usd;
        if c > 0.0 && (limit == 0.0 || limit > c) {
            return Err(format!(
                "BudgetCeiling: cost.max_session_usd is ${c:.2}; the cap can be lowered, not raised or removed"
            ));
        }
        self.limit_usd = limit;
        Ok(limit)
    }

    fn record_spend(&mut self, usd: f64) {
        self.spent_usd += usd;
    }
}

impl Default for TurnBudget {
    fn default() -> Self {
        Self {
            consecutive_actions: 0,
            max_consecutive_actions: MAX_CONSECUTIVE_ACTIONS,
            parse_retries: 0,
            total_failures: 0,
            spent_usd: 0.0,
            limit_usd: 0.0,
            ceiling_usd: 0.0,
        }
    }
}

impl TurnBudget {
    pub fn new(spent_usd: f64, limit_usd: f64) -> Self {
        Self {
            spent_usd,
            limit_usd,
            ..Default::default()
        }
    }

    /// Applies `cost.max_session_usd`: a missing or larger cap becomes the ceiling.
    pub fn with_ceiling(mut self, ceiling_usd: f64) -> Self {
        if ceiling_usd > 0.0 {
            self.ceiling_usd = ceiling_usd;
            if self.limit_usd <= 0.0 || self.limit_usd > ceiling_usd {
                self.limit_usd = ceiling_usd;
            }
        }
        self
    }

    /// Called before dispatching an action. `None` means proceed.
    pub fn check(&self) -> Option<BudgetStop> {
        // Money first: it is the only limit that costs the user something real
        // to overrun, and it is the one that was never enforced.
        if self.limit_usd > 0.0 && self.spent_usd >= self.limit_usd {
            return Some(BudgetStop::CostExhausted {
                spent_usd: self.spent_usd,
                limit_usd: self.limit_usd,
            });
        }
        if self.total_failures >= MAX_TOTAL_FAILURES {
            return Some(BudgetStop::TotalFailures);
        }
        if self.parse_retries >= MAX_PARSE_RETRIES {
            return Some(BudgetStop::ParseRetries);
        }
        if self.max_consecutive_actions > 0
            && self.consecutive_actions >= self.max_consecutive_actions
        {
            return Some(BudgetStop::ConsecutiveActions);
        }
        None
    }

    /// An action ran and succeeded. Forward progress clears the failure
    /// counters — a model that recovers should not be punished for how long
    /// it took, only for failing to recover at all.
    pub fn record_success(&mut self) {
        self.consecutive_actions += 1;
        self.parse_retries = 0;
    }

    /// An action ran and failed.
    ///
    /// Only the quantitative counters move. Whether this is the *same* action
    /// failing again is a question about behaviour, and
    /// `agent::loop_detect` owns it — keyed on identity and arguments rather
    /// than on the action name, so a model fixing six different paths is not
    /// mistaken for a model stuck on one.
    pub fn record_action_failure(&mut self) {
        self.consecutive_actions += 1;
        self.total_failures += 1;
    }

    /// The model's output would not parse.
    pub fn record_parse_failure(&mut self) {
        self.parse_retries += 1;
        self.total_failures += 1;
    }

    /// Money spent on a model call.
    pub fn record_spend(&mut self, usd: f64) {
        self.spent_usd += usd;
    }

    /// A new user turn: per-turn counters reset, session spend does not.
    pub fn start_turn(&mut self) {
        self.consecutive_actions = 0;
        self.parse_retries = 0;
        self.total_failures = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_turn_may_proceed() {
        assert_eq!(TurnBudget::default().check(), None);
    }

    /// The limit serve advertised and never applied. A session at or over its
    /// ceiling must stop before spending more, not after.
    #[test]
    fn the_monetary_ceiling_is_enforced() {
        let b = TurnBudget::new(2.0, 2.0);
        assert_eq!(
            b.check(),
            Some(BudgetStop::CostExhausted {
                spent_usd: 2.0,
                limit_usd: 2.0
            })
        );
    }

    #[test]
    fn a_zero_limit_means_no_monetary_ceiling() {
        let b = TurnBudget::new(999.0, 0.0);
        assert_eq!(b.check(), None, "0.0 is the config default for 'unlimited'");
    }

    /// The budget counts failures; it does not judge whether they are the
    /// *same* failure. That question moved to `agent::loop_detect`, which
    /// keys on identity and arguments — `WriteFile` failing on six different
    /// paths is progress, and a name-keyed counter here would have stopped it.
    /// Only the absolute ceiling binds.
    #[test]
    fn failures_accumulate_toward_the_total_ceiling_only() {
        let mut b = TurnBudget::default();
        for _ in 0..MAX_TOTAL_FAILURES - 1 {
            b.record_action_failure();
        }
        assert_eq!(b.check(), None, "below the ceiling the turn continues");
        b.record_action_failure();
        assert_eq!(b.check(), Some(BudgetStop::TotalFailures));
    }

    #[test]
    fn success_clears_the_parse_retry_counter() {
        let mut b = TurnBudget::default();
        for _ in 0..MAX_PARSE_RETRIES - 1 {
            b.record_parse_failure();
        }
        b.record_success();
        b.record_parse_failure();
        assert_eq!(b.check(), None, "recovery must not be punished");
    }

    /// Total failures survive a success: they bound the turn, not a streak.
    /// Clearing them on any success would let a turn fail indefinitely as long
    /// as something occasionally worked.
    #[test]
    fn success_does_not_clear_the_total_failure_ceiling() {
        let mut b = TurnBudget::default();
        for _ in 0..MAX_TOTAL_FAILURES {
            b.record_action_failure();
            b.record_success();
        }
        assert_eq!(b.check(), Some(BudgetStop::TotalFailures));
    }

    /// The live session that motivated the parser fix: five identical parse
    /// failures, 28.6k tokens. The cap is what bounds that.
    #[test]
    fn unparseable_output_stops_after_the_retry_budget() {
        let mut b = TurnBudget::default();
        for _ in 0..MAX_PARSE_RETRIES {
            b.record_parse_failure();
        }
        assert_eq!(b.check(), Some(BudgetStop::ParseRetries));
    }

    #[test]
    fn a_new_turn_clears_per_turn_counters_but_not_spend() {
        let mut b = TurnBudget::new(1.5, 10.0);
        for _ in 0..MAX_PARSE_RETRIES {
            b.record_parse_failure();
        }
        b.start_turn();
        assert_eq!(b.check(), None);
        assert_eq!(b.spent_usd, 1.5, "the bill does not reset with the turn");
    }

    #[test]
    fn every_stop_explains_itself() {
        for stop in [
            BudgetStop::ConsecutiveActions,
            BudgetStop::ParseRetries,
            BudgetStop::TotalFailures,
            BudgetStop::CostExhausted {
                spent_usd: 1.0,
                limit_usd: 1.0,
            },
        ] {
            let m = stop.message();
            assert!(m.len() > 20, "unhelpfully terse: {m}");
        }
    }
}

/// `/budget`, over live session state instead of configuration.
///
/// The historical command reads and writes `config.cost.budget_usd_limit`
/// (`slash.rs:452-480`). On the canonical path that number is the session's
/// *initial* value and nothing consults it afterwards: enforcement reads
/// `AgentState.budget` (`driver.rs:213`), so the old command would print a
/// figure the driver ignores and set a cap that never applies. Same words,
/// same three forms, live number.
///
/// Returns the line to print rather than printing it: the frontend owns the
/// terminal, and a function that returns text can be tested.
pub fn slash_budget(arg: &str, live: &mut dyn BudgetControl) -> Result<String, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        let cap = live.current_limit_usd();
        return Ok(if cap <= 0.0 {
            "budget guard: disabled (set with /budget <usd>)".to_string()
        } else {
            format!(
                "budget cap: ${:.2} per session (${:.2} spent)",
                cap,
                live.spent_usd()
            )
        });
    }
    if arg.eq_ignore_ascii_case("off") {
        live.set_limit_usd(0.0)?;
        return Ok("budget guard disabled".to_string());
    }
    match arg.parse::<f64>() {
        Ok(v) => {
            let applied = live.set_limit_usd(v)?;
            Ok(format!("budget cap set to ${applied:.2}"))
        }
        // The historical wording, so a user who learned this message does not
        // have to learn another.
        Err(_) => Err(format!(
            "invalid budget '{arg}' — use a positive number or 'off'"
        )),
    }
}

#[cfg(test)]
mod slash_budget_tests {
    use super::*;

    #[test]
    fn the_three_historical_forms_read_and_write_the_live_limit() {
        let mut b = TurnBudget {
            limit_usd: 0.0,
            spent_usd: 2.5,
            ..Default::default()
        };
        assert!(slash_budget("", &mut b).unwrap().contains("disabled"));

        let out = slash_budget("25", &mut b).unwrap();
        assert_eq!(out, "budget cap set to $25.00");
        assert_eq!(b.limit_usd, 25.0);
        // Setting a ceiling does not rewrite what was already spent.
        assert_eq!(b.spent_usd, 2.5);

        let shown = slash_budget("", &mut b).unwrap();
        assert!(
            shown.contains("$25.00") && shown.contains("$2.50"),
            "{shown}"
        );

        assert_eq!(
            slash_budget("off", &mut b).unwrap(),
            "budget guard disabled"
        );
        assert_eq!(b.limit_usd, 0.0);
    }

    #[test]
    fn a_nonsense_argument_is_refused_in_the_historical_wording() {
        let mut b = TurnBudget::default();
        let err = slash_budget("lots", &mut b).unwrap_err();
        assert!(err.contains("invalid budget 'lots'"), "{err}");
        assert!(err.contains("positive number or 'off'"), "{err}");
        assert_eq!(b.limit_usd, 0.0, "a refused argument still changed the cap");
    }
}

#[cfg(test)]
mod ceiling_tests {
    use super::*;

    #[test]
    fn the_config_ceiling_cannot_be_raised_or_removed() {
        let mut b = TurnBudget::new(0.0, 0.0).with_ceiling(5.0);
        assert_eq!(b.current_limit_usd(), 5.0, "no cap: the ceiling applies");
        for v in [1e9, 5.01, 0.0] {
            let e = b.set_limit_usd(v).unwrap_err();
            assert!(e.starts_with("BudgetCeiling"), "{v}: {e}");
        }
        assert_eq!(b.set_limit_usd(2.0), Ok(2.0));
        assert!(slash_budget("off", &mut b).is_err());
        assert_eq!(TurnBudget::new(0.0, 50.0).with_ceiling(5.0).limit_usd, 5.0);
        assert_eq!(
            TurnBudget::new(0.0, 0.0)
                .with_ceiling(0.0)
                .set_limit_usd(1e9),
            Ok(1e9)
        );
    }
}
