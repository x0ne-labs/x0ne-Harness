//! Four-dimensional hard budget. A child's allowance is DEBITED from its
//! parent, never copied: the sum over the tree is bounded by the root.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: u64,
    pub micro_usd: u64,
    pub ms: u64,
    pub actions: u64,
}

impl Budget {
    /// Every dimension of `self` fits inside `other`.
    pub fn fits_in(&self, other: &Budget) -> bool {
        self.tokens <= other.tokens
            && self.micro_usd <= other.micro_usd
            && self.ms <= other.ms
            && self.actions <= other.actions
    }

    pub fn plus(&self, other: &Budget) -> Budget {
        Budget {
            tokens: self.tokens.saturating_add(other.tokens),
            micro_usd: self.micro_usd.saturating_add(other.micro_usd),
            ms: self.ms.saturating_add(other.ms),
            actions: self.actions.saturating_add(other.actions),
        }
    }

    /// `self - other`, or the first dimension that would go negative.
    pub fn minus(&self, other: &Budget) -> Result<Budget, &'static str> {
        Ok(Budget {
            tokens: self.tokens.checked_sub(other.tokens).ok_or("tokens")?,
            micro_usd: self
                .micro_usd
                .checked_sub(other.micro_usd)
                .ok_or("micro_usd")?,
            ms: self.ms.checked_sub(other.ms).ok_or("ms")?,
            actions: self.actions.checked_sub(other.actions).ok_or("actions")?,
        })
    }

    pub fn actions(n: u64) -> Budget {
        Budget {
            actions: n,
            ..Budget::default()
        }
    }

    pub fn ms(n: u64) -> Budget {
        Budget {
            ms: n,
            ..Budget::default()
        }
    }
}

/// Per-node accounting. `initial = remaining + spent + lent` at all times.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    pub initial: Budget,
    pub remaining: Budget,
    pub spent: Budget,
    /// Currently held by running children; returns on their completion.
    pub lent: Budget,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BudgetError {
    #[error("budget exhausted: {0}")]
    Exhausted(&'static str),
    #[error("budget exceeds parent remaining: {0}")]
    ExceedsParent(&'static str),
}

impl Ledger {
    pub fn new(initial: Budget) -> Self {
        Ledger {
            initial,
            remaining: initial,
            ..Ledger::default()
        }
    }

    pub fn debit(&mut self, cost: Budget) -> Result<(), BudgetError> {
        self.remaining = self
            .remaining
            .minus(&cost)
            .map_err(BudgetError::Exhausted)?;
        self.spent = self.spent.plus(&cost);
        Ok(())
    }

    /// Hand `child` to a child: it leaves `remaining` and sits in `lent`.
    /// A child with zero actions could never report, so it is refused.
    pub fn lend(&mut self, child: Budget) -> Result<(), BudgetError> {
        if child.actions == 0 {
            return Err(BudgetError::ExceedsParent("actions must be >= 1"));
        }
        self.remaining = self
            .remaining
            .minus(&child)
            .map_err(BudgetError::ExceedsParent)?;
        self.lent = self.lent.plus(&child);
        Ok(())
    }

    /// A child finished: what it did not spend comes back, what it spent
    /// leaves `lent` for good (it is accounted in the child's own `spent`).
    pub fn settle(&mut self, child_initial: Budget, child_unspent: Budget) {
        self.lent = self.lent.minus(&child_initial).unwrap_or_default();
        self.remaining = self.remaining.plus(&child_unspent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lend_then_settle_conserves() {
        let mut l = Ledger::new(Budget {
            tokens: 100,
            micro_usd: 100,
            ms: 100,
            actions: 10,
        });
        l.debit(Budget::actions(1)).unwrap();
        let child = Budget {
            tokens: 50,
            micro_usd: 50,
            ms: 50,
            actions: 4,
        };
        l.lend(child).unwrap();
        assert_eq!(l.remaining.actions, 5);
        l.settle(
            child,
            Budget {
                tokens: 10,
                micro_usd: 10,
                ms: 10,
                actions: 1,
            },
        );
        assert_eq!(l.remaining.actions, 6);
        assert_eq!(l.lent, Budget::default());
        assert_eq!(l.remaining.plus(&l.spent).actions + 3, l.initial.actions);
    }

    #[test]
    fn lend_refuses_more_than_remaining_and_zero_actions() {
        let mut l = Ledger::new(Budget {
            tokens: 10,
            micro_usd: 10,
            ms: 10,
            actions: 2,
        });
        assert_eq!(
            l.lend(Budget {
                tokens: 11,
                ..Budget::actions(1)
            }),
            Err(BudgetError::ExceedsParent("tokens"))
        );
        assert!(l.lend(Budget::default()).is_err());
        assert!(l.lend(Budget::actions(2)).is_ok());
        assert_eq!(l.remaining.actions, 0);
        assert_eq!(
            l.debit(Budget::actions(1)),
            Err(BudgetError::Exhausted("actions"))
        );
    }
}
