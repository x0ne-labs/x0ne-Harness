//! The orchestrator: owns every node record, key, ledger, sandbox and
//! journal. Nodes decide; the tree checks, executes, verifies and merges.
//! Nothing a node emits is trusted: not its caps request, its budget
//! request, its identity claim, its "verified" claim, nor its diff.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;

use crate::budget::{Budget, BudgetError, Ledger};
use crate::caps::{CapSet, Capability};
use crate::identity::{NodeKey, Signed};
use crate::journal::{Event, Journal};
use crate::llm::{Broker, Decision, LlmBackend, Observation};
use crate::merge::{merge_into, MergePolicy, MergeReport};
use crate::node::{normalise_statement, NodeId, NodeState, NodeView, TaskSpec};
use crate::report::{bound, ChildReport, Report};
use crate::sandbox::{CancelFlag, Sandbox, SandboxPool};

#[derive(Clone, Debug)]
pub struct TreeConfig {
    /// Simultaneous children per node (width).
    pub max_children_per_node: usize,
    /// Simultaneously RUNNING sandboxes over the whole tree; the rest queue.
    pub max_concurrent_sandboxes: usize,
    pub max_summary_chars: usize,
    pub max_evidence_chars: usize,
    /// A statement spawned this many times anywhere in the tree is a loop.
    pub max_same_statement: usize,
    pub exec_timeout: Duration,
    pub merge_policy: MergePolicy,
    /// Deepest allowed node (root = 0). `None`: the budget alone bounds depth.
    pub max_depth: Option<usize>,
}

impl Default for TreeConfig {
    fn default() -> Self {
        TreeConfig {
            max_children_per_node: 8,
            max_concurrent_sandboxes: 4,
            max_summary_chars: 2000,
            max_evidence_chars: 1000,
            max_same_statement: 3,
            exec_timeout: Duration::from_secs(30),
            merge_policy: MergePolicy::default(),
            max_depth: None,
        }
    }
}

struct NodeRecord {
    id: NodeId,
    parent: Option<NodeId>,
    spec: TaskSpec,
    statement_key: String,
    state: NodeState,
    ledger: Ledger,
    key: NodeKey,
    cancel: CancelFlag,
    sandbox: Arc<Sandbox>,
    children: Vec<NodeId>,
    collected: usize,
    report: Option<ChildReport>,
}

/// Read-only copy for inspection and tests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSnapshot {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub depth: usize,
    pub state: NodeState,
    pub caps: CapSet,
    pub ledger: Ledger,
    pub report: Option<ChildReport>,
}

#[derive(Clone, Debug)]
pub struct RootOutcome {
    pub verified: bool,
    pub state: NodeState,
    pub report: Option<ChildReport>,
}

struct Inner {
    cfg: TreeConfig,
    journal: Journal,
    pool: SandboxPool,
    broker: Broker,
    host_workspace: PathBuf,
    nodes: Mutex<BTreeMap<NodeId, NodeRecord>>,
    joins: Mutex<HashMap<NodeId, JoinHandle<()>>>,
    statements: Mutex<HashMap<String, usize>>,
}

#[derive(Clone)]
pub struct Tree(Arc<Inner>);

enum Finish {
    Reported(Report),
    Failed(String),
    Cancelled,
}

impl Tree {
    pub fn new(
        cfg: TreeConfig,
        journal_dir: &Path,
        sandbox_dir: &Path,
        host_workspace: &Path,
        backend: Arc<dyn LlmBackend>,
        api_key: String,
    ) -> Result<Tree, String> {
        let journal = Journal::open(journal_dir).map_err(|e| e.to_string())?;
        let pool = SandboxPool::new(sandbox_dir, cfg.max_concurrent_sandboxes)?;
        Ok(Tree(Arc::new(Inner {
            cfg,
            journal,
            pool,
            broker: Broker::new(backend, api_key),
            host_workspace: host_workspace.to_path_buf(),
            nodes: Mutex::new(BTreeMap::new()),
            joins: Mutex::new(HashMap::new()),
            statements: Mutex::new(HashMap::new()),
        })))
    }

    pub fn journal(&self) -> &Journal {
        &self.0.journal
    }

    fn jl(&self, id: &NodeId, kind: &str, data: serde_json::Value) {
        if let Err(e) = self.0.journal.append(id, Event::new(kind, data)) {
            eprintln!("journal {id} {kind}: {e}");
        }
    }

    fn with_node<R>(&self, id: &NodeId, f: impl FnOnce(&mut NodeRecord) -> R) -> Option<R> {
        let mut nodes = self.0.nodes.lock().expect("nodes lock");
        nodes.get_mut(id).map(f)
    }

    pub fn snapshot(&self) -> BTreeMap<NodeId, NodeSnapshot> {
        let nodes = self.0.nodes.lock().expect("nodes lock");
        nodes
            .values()
            .map(|r| {
                (
                    r.id.clone(),
                    NodeSnapshot {
                        id: r.id.clone(),
                        parent: r.parent.clone(),
                        depth: r.id.depth(),
                        state: r.state.clone(),
                        caps: r.spec.caps.clone(),
                        ledger: r.ledger,
                        report: r.report.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn total_spent(&self) -> Budget {
        let nodes = self.0.nodes.lock().expect("nodes lock");
        nodes
            .values()
            .fold(Budget::default(), |acc, r| acc.plus(&r.ledger.spent))
    }

    /// Every child's capability set is a subset of its parent's.
    pub fn caps_monotone(&self) -> bool {
        let nodes = self.0.nodes.lock().expect("nodes lock");
        nodes.values().all(|r| match &r.parent {
            None => true,
            Some(p) => nodes
                .get(p)
                .map(|pr| r.spec.caps.is_subset_of(&pr.spec.caps))
                .unwrap_or(false),
        })
    }

    pub fn root_workdir(&self) -> Option<PathBuf> {
        self.with_node(&NodeId::root(), |r| r.sandbox.work.clone())
    }

    /// The only path to the host: the root's verified diff, policy-filtered.
    pub fn export_root(&self) -> Result<MergeReport, String> {
        let (state, sb) = self
            .with_node(&NodeId::root(), |r| {
                (r.state.clone(), Arc::clone(&r.sandbox))
            })
            .ok_or("no root")?;
        if state != NodeState::Verified {
            return Err(format!(
                "root is {:?}, not verified: nothing leaves the sandbox",
                state
            ));
        }
        let rep = merge_into(
            &sb.base,
            &sb.work,
            &self.0.host_workspace,
            &self.0.cfg.merge_policy,
        )?;
        self.jl(
            &NodeId::root(),
            "export",
            json!({"applied": rep.applied, "rejected": rep.rejected, "conflicts": rep.conflicts}),
        );
        Ok(rep)
    }

    /// Cancel a node and its whole subtree. Budgets settle as nodes exit.
    pub fn cancel(&self, id: &NodeId) -> Result<(), String> {
        let flags: Vec<(NodeId, CancelFlag)> = {
            let nodes = self.0.nodes.lock().expect("nodes lock");
            if !nodes.contains_key(id) {
                return Err(format!("unknown node {id}"));
            }
            nodes
                .values()
                .filter(|r| r.id == *id || id.is_ancestor_of(&r.id))
                .map(|r| (r.id.clone(), r.cancel.clone()))
                .collect()
        };
        self.jl(id, "cancel_requested", json!({"subtree": flags.len()}));
        for (_, f) in &flags {
            f.cancel();
        }
        Ok(())
    }

    /// The message boundary: a report counts only under the node's own key.
    pub fn accept_report(&self, signed: Signed<ChildReport>) -> Result<(), String> {
        let id = signed.node.clone();
        let key = self
            .with_node(&id, |r| r.key.clone())
            .ok_or_else(|| format!("unknown node {id}"))?;
        if signed.payload.node != id {
            self.jl(
                &id,
                "forged_message",
                json!({"reason": "payload node mismatch", "claimed": signed.payload.node}),
            );
            return Err("payload node mismatch".into());
        }
        if !signed.verify(&key) {
            self.jl(&id, "forged_message", json!({"reason": "bad mac"}));
            return Err("bad mac: report not signed by this node's key".into());
        }
        let stored = self
            .with_node(&id, |r| {
                if r.report.is_some() {
                    false
                } else {
                    r.report = Some(signed.payload.clone());
                    true
                }
            })
            .unwrap_or(false);
        if !stored {
            self.jl(
                &id,
                "forged_message",
                json!({"reason": "report already accepted"}),
            );
            return Err("report already accepted".into());
        }
        self.jl(&id, "report_accepted", json!({"verified": signed.payload.verified, "summary_chars": signed.payload.summary.chars().count()}));
        Ok(())
    }

    pub async fn run_root(&self, spec: TaskSpec) -> Result<RootOutcome, String> {
        let id = NodeId::root();
        if self.0.nodes.lock().expect("nodes lock").contains_key(&id) {
            return Err("root already ran".into());
        }
        let sandbox = self.0.pool.clone_from(&id, &self.0.host_workspace)?;
        self.insert(id.clone(), None, spec);
        self.with_node(&id, |r| r.sandbox = Arc::new(sandbox));
        self.clone().run_node(id.clone()).await;
        let (state, report) = self
            .with_node(&id, |r| (r.state.clone(), r.report.clone()))
            .ok_or("root vanished")?;
        Ok(RootOutcome {
            verified: state == NodeState::Verified,
            state,
            report,
        })
    }

    fn insert(&self, id: NodeId, parent: Option<NodeId>, spec: TaskSpec) {
        let key = normalise_statement(&spec.statement);
        *self
            .0
            .statements
            .lock()
            .expect("statements lock")
            .entry(key.clone())
            .or_insert(0) += 1;
        self.jl(
            &id,
            "spawned",
            json!({
                "statement": spec.statement, "caps": spec.caps, "budget": spec.budget,
                "verify": spec.verify, "model": spec.model,
            }),
        );
        let placeholder = Arc::new(Sandbox {
            id: id.clone(),
            work: PathBuf::new(),
            base: PathBuf::new(),
            bwrap: PathBuf::new(),
        });
        let rec = NodeRecord {
            id: id.clone(),
            parent,
            ledger: Ledger::new(spec.budget),
            statement_key: key,
            spec,
            state: NodeState::Queued,
            key: NodeKey::generate(),
            cancel: CancelFlag::default(),
            sandbox: placeholder,
            children: Vec::new(),
            collected: 0,
            report: None,
        };
        self.0.nodes.lock().expect("nodes lock").insert(id, rec);
    }

    fn view(&self, id: &NodeId) -> NodeView {
        self.with_node(id, |r| NodeView {
            id: r.id.clone(),
            depth: r.id.depth(),
            statement: r.spec.statement.clone(),
            caps: r.spec.caps.clone(),
            remaining: r.ledger.remaining,
            model: r.spec.model,
        })
        .expect("node exists")
    }

    fn debit(&self, id: &NodeId, cost: Budget) -> Result<(), BudgetError> {
        self.with_node(id, |r| r.ledger.debit(cost))
            .expect("node exists")
    }

    /// Debit at most what is left; returns the part that did not fit.
    /// Only for usage already billed, which must be accounted for anyway.
    fn debit_clamped(&self, id: &NodeId, cost: Budget) -> Budget {
        self.with_node(id, |r| {
            let rem = r.ledger.remaining;
            let take = Budget {
                tokens: cost.tokens.min(rem.tokens),
                micro_usd: cost.micro_usd.min(rem.micro_usd),
                ms: cost.ms.min(rem.ms),
                actions: cost.actions.min(rem.actions),
            };
            let _ = r.ledger.debit(take);
            cost.minus(&take).unwrap_or_default()
        })
        .unwrap_or(cost)
    }

    fn is_cancelled(&self, id: &NodeId) -> bool {
        self.with_node(id, |r| r.cancel.is_cancelled())
            .unwrap_or(true)
    }

    fn set_state(&self, id: &NodeId, s: NodeState) {
        self.with_node(id, |r| r.state = s);
    }

    /// All checks happen here, outside the requesting node's sandbox.
    fn try_spawn(&self, parent: &NodeId, spec: TaskSpec) -> Result<NodeId, String> {
        let key = normalise_statement(&spec.statement);
        let (child_id, parent_work) = {
            let mut nodes = self.0.nodes.lock().expect("nodes lock");
            let ancestors: Vec<String> = parent
                .ancestors()
                .iter()
                .filter_map(|a| nodes.get(a).map(|r| r.statement_key.clone()))
                .collect();
            let global = self
                .0
                .statements
                .lock()
                .expect("statements lock")
                .get(&key)
                .copied()
                .unwrap_or(0);
            let p = nodes.get_mut(parent).ok_or("unknown parent")?;
            if p.cancel.is_cancelled() {
                return Err("parent cancelled".into());
            }
            if let Some(max) = self.0.cfg.max_depth {
                if parent.depth() + 1 > max {
                    return Err(format!("depth limit reached ({max})"));
                }
            }
            if !p.spec.caps.has(Capability::Spawn) {
                return Err("caps: parent does not hold Spawn".into());
            }
            if !spec.caps.is_subset_of(&p.spec.caps) {
                return Err("caps not a subset of the parent's: capabilities never widen".into());
            }
            if p.children.len() >= self.0.cfg.max_children_per_node {
                return Err(format!(
                    "width limit reached ({})",
                    self.0.cfg.max_children_per_node
                ));
            }
            if key == p.statement_key || ancestors.contains(&key) {
                return Err(
                    "loop detected: this statement is already running in the ancestry".into(),
                );
            }
            if global >= self.0.cfg.max_same_statement {
                return Err(format!(
                    "loop detected: statement already spawned {global} times"
                ));
            }
            p.ledger.lend(spec.budget).map_err(|e| e.to_string())?;
            let n = p.children.len() as u32 + 1;
            let cid = parent.child(n);
            p.children.push(cid.clone());
            (cid, p.sandbox.work.clone())
        };
        let sandbox = match self.0.pool.clone_from(&child_id, &parent_work) {
            Ok(s) => s,
            Err(e) => {
                self.with_node(parent, |p| {
                    p.ledger.settle(spec.budget, spec.budget);
                    p.children.retain(|c| *c != child_id);
                });
                return Err(format!("sandbox clone failed: {e}"));
            }
        };
        self.insert(child_id.clone(), Some(parent.clone()), spec);
        self.with_node(&child_id, |r| r.sandbox = Arc::new(sandbox));
        self.jl(parent, "spawn_accepted", json!({"child": child_id}));
        let t = self.clone();
        let cid = child_id.clone();
        let handle = tokio::spawn(Box::pin(async move { t.run_node(cid).await }));
        self.0
            .joins
            .lock()
            .expect("joins lock")
            .insert(child_id.clone(), handle);
        Ok(child_id)
    }

    /// Await every not-yet-collected child, merge the verified ones into the
    /// parent's clone, and return their bounded reports as data.
    async fn collect_children(&self, id: &NodeId) -> Vec<ChildReport> {
        let pending: Vec<NodeId> = self
            .with_node(id, |r| {
                let v = r.children[r.collected..].to_vec();
                r.collected = r.children.len();
                v
            })
            .unwrap_or_default();
        let mut out = Vec::new();
        for cid in pending {
            let handle = self.0.joins.lock().expect("joins lock").remove(&cid);
            if let Some(h) = handle {
                let _ = h.await;
            }
            let (state, report, csb) = self
                .with_node(&cid, |r| {
                    (r.state.clone(), r.report.clone(), Arc::clone(&r.sandbox))
                })
                .expect("child exists");
            if state == NodeState::Verified {
                let psb = self
                    .with_node(id, |r| Arc::clone(&r.sandbox))
                    .expect("parent exists");
                match merge_into(&csb.base, &csb.work, &psb.work, &self.0.cfg.merge_policy) {
                    Ok(rep) => {
                        self.jl(&cid, "merge", json!({"into": id, "applied": rep.applied, "rejected": rep.rejected.len(), "conflicts": rep.conflicts}));
                        for (p, why) in &rep.rejected {
                            self.jl(&cid, "merge_rejected", json!({"path": p, "reason": why}));
                        }
                        for p in &rep.conflicts {
                            self.jl(&cid, "merge_conflict", json!({"path": p, "kept": "parent"}));
                        }
                    }
                    Err(e) => self.jl(&cid, "merge_failed", json!({"error": e})),
                }
            }
            out.push(report.unwrap_or(ChildReport {
                node: cid.clone(),
                state: state.label(),
                verified: false,
                summary: String::new(),
                evidence: String::new(),
            }));
        }
        out
    }

    async fn run_node(self, id: NodeId) {
        let mut obs: Vec<Observation> = Vec::new();
        let mut permit: Option<OwnedSemaphorePermit> = Some(self.0.pool.acquire().await);
        self.set_state(&id, NodeState::Running);
        self.jl(&id, "running", json!({}));
        let finish = loop {
            if self.is_cancelled(&id) {
                break Finish::Cancelled;
            }
            if let Err(e) = self.debit(&id, Budget::actions(1)) {
                break Finish::Failed(e.to_string());
            }
            let view = self.view(&id);
            let (decision, usage) = match self.0.broker.next(&view, &obs).await {
                Ok(x) => x,
                Err(e) => {
                    // A failed call can still be billed: debit what was used.
                    if e.usage != Default::default() {
                        self.jl(&id, "llm_call", json!({"backend": self.0.broker.backend_name(), "model": view.model, "tokens": e.usage.tokens, "micro_usd": e.usage.micro_usd, "error": true}));
                        let _ = self.debit_clamped(
                            &id,
                            Budget {
                                tokens: e.usage.tokens,
                                micro_usd: e.usage.micro_usd,
                                ..Budget::default()
                            },
                        );
                    }
                    break Finish::Failed(format!("llm: {e}"));
                }
            };
            self.jl(&id, "llm_call", json!({"backend": self.0.broker.backend_name(), "model": view.model, "tokens": usage.tokens, "micro_usd": usage.micro_usd}));
            // Already billed: account for all of it, then stop if it overran.
            let over = self.debit_clamped(
                &id,
                Budget {
                    tokens: usage.tokens,
                    micro_usd: usage.micro_usd,
                    ..Budget::default()
                },
            );
            if over != Budget::default() {
                self.jl(&id, "overspend", json!({"beyond_budget": over}));
                break Finish::Failed("budget exhausted by an LLM call".into());
            }
            let ob = match decision {
                Decision::Spawn(spec) => match self.try_spawn(&id, spec) {
                    Ok(cid) => Observation::SpawnAccepted {
                        id: cid.to_string(),
                    },
                    Err(reason) => {
                        self.jl(&id, "spawn_rejected", json!({"reason": reason}));
                        Observation::SpawnRejected { reason }
                    }
                },
                Decision::Exec { cmd } => {
                    if !view.caps.has(Capability::Exec) {
                        self.jl(&id, "policy_denied", json!({"what": "exec", "cmd": cmd, "reason": "capability Exec not held"}));
                        Observation::Denied {
                            what: "exec".into(),
                            reason: "capability Exec not held".into(),
                        }
                    } else {
                        let (sb, cancel) = self
                            .with_node(&id, |r| (Arc::clone(&r.sandbox), r.cancel.clone()))
                            .expect("node");
                        let timeout = self
                            .0
                            .cfg
                            .exec_timeout
                            .min(Duration::from_millis(view.remaining.ms.max(1)));
                        let r = sb.exec(&cmd, timeout, &cancel).await;
                        self.jl(&id, "exec", json!({
                            "cmd": cmd, "exit": r.exit, "stdout": bound(&r.stdout, 4000), "stderr": bound(&r.stderr, 2000),
                            "elapsed_ms": r.elapsed_ms, "timed_out": r.timed_out, "cancelled": r.cancelled,
                        }));
                        if let Err(e) = self.debit(&id, Budget::ms(r.elapsed_ms)) {
                            break Finish::Failed(e.to_string());
                        }
                        Observation::Exec {
                            cmd,
                            exit: r.exit,
                            stdout: bound(&r.stdout, 8000),
                            stderr: bound(&r.stderr, 4000),
                        }
                    }
                }
                Decision::Write { path, content } => {
                    if !view.caps.has(Capability::Write) {
                        self.jl(&id, "policy_denied", json!({"what": "write", "path": path, "reason": "capability Write not held"}));
                        Observation::Denied {
                            what: "write".into(),
                            reason: "capability Write not held".into(),
                        }
                    } else {
                        let sb = self
                            .with_node(&id, |r| Arc::clone(&r.sandbox))
                            .expect("node");
                        match sb.write(&path, &content) {
                            Ok(_) => {
                                self.jl(
                                    &id,
                                    "write",
                                    json!({"path": path, "bytes": content.len()}),
                                );
                                Observation::Wrote { path }
                            }
                            Err(reason) => {
                                self.jl(
                                    &id,
                                    "policy_denied",
                                    json!({"what": "write", "path": path, "reason": reason}),
                                );
                                Observation::Denied {
                                    what: "write".into(),
                                    reason,
                                }
                            }
                        }
                    }
                }
                Decision::Read { path } => {
                    if !view.caps.has(Capability::Read) {
                        self.jl(&id, "policy_denied", json!({"what": "read", "path": path, "reason": "capability Read not held"}));
                        Observation::Denied {
                            what: "read".into(),
                            reason: "capability Read not held".into(),
                        }
                    } else {
                        let sb = self
                            .with_node(&id, |r| Arc::clone(&r.sandbox))
                            .expect("node");
                        match sb.read(&path) {
                            Ok(content) => {
                                self.jl(&id, "read", json!({"path": path, "bytes": content.len()}));
                                Observation::Read {
                                    path,
                                    content: bound(&content, 8000),
                                }
                            }
                            Err(reason) => {
                                self.jl(
                                    &id,
                                    "policy_denied",
                                    json!({"what": "read", "path": path, "reason": reason}),
                                );
                                Observation::Denied {
                                    what: "read".into(),
                                    reason,
                                }
                            }
                        }
                    }
                }
                Decision::Wait => {
                    drop(permit.take());
                    let reports = self.collect_children(&id).await;
                    permit = Some(self.0.pool.acquire().await);
                    Observation::Children(reports)
                }
                Decision::Report(r) => {
                    if let Some(claimed) = &r.as_node {
                        if *claimed != id {
                            self.jl(
                                &id,
                                "forged_identity",
                                json!({"claimed": claimed, "actual": id}),
                            );
                            Observation::Denied {
                                what: "report".into(),
                                reason: format!("you are {id}, not {claimed}"),
                            }
                        } else {
                            drop(permit.take());
                            break Finish::Reported(r);
                        }
                    } else {
                        drop(permit.take());
                        break Finish::Reported(r);
                    }
                }
            };
            obs.push(ob);
        };
        drop(permit);
        self.finalize(&id, finish).await;
    }

    async fn finalize(&self, id: &NodeId, finish: Finish) {
        let (state, report) = match finish {
            Finish::Reported(r) => {
                // Children must be in before the verifier looks at the clone.
                let _ = self.collect_children(id).await;
                let (verify, sb, cancel) = self
                    .with_node(id, |r| {
                        (
                            r.spec.verify.clone(),
                            Arc::clone(&r.sandbox),
                            r.cancel.clone(),
                        )
                    })
                    .expect("node");
                let (state, verified, evidence) = match verify {
                    None => (NodeState::Unverified, false, String::new()),
                    Some(cmd) => {
                        let _permit = self.0.pool.acquire().await;
                        let res = sb.exec(&cmd, self.0.cfg.exec_timeout, &cancel).await;
                        let ok = res.exit == 0 && !res.timed_out && !res.cancelled;
                        let evidence = bound(
                            &format!("$ {cmd}\nexit {}\n{}{}", res.exit, res.stdout, res.stderr),
                            self.0.cfg.max_evidence_chars,
                        );
                        self.jl(id, "verdict", json!({"verified": ok, "cmd": cmd, "exit": res.exit, "evidence": evidence}));
                        if ok {
                            (NodeState::Verified, true, evidence)
                        } else {
                            (NodeState::Failed("verifier failed".into()), false, evidence)
                        }
                    }
                };
                if r.claims_verified && !verified {
                    self.jl(
                        id,
                        "claim_ignored",
                        json!({"claimed_verified": true, "verified": false}),
                    );
                }
                let report = ChildReport {
                    node: id.clone(),
                    state: state.label(),
                    verified,
                    summary: bound(&r.summary, self.0.cfg.max_summary_chars),
                    evidence,
                };
                let key = self.with_node(id, |r| r.key.clone()).expect("node");
                let signed = Signed::sign(id.clone(), &key, report.clone());
                if let Err(e) = self.accept_report(signed) {
                    self.jl(id, "report_rejected", json!({"error": e}));
                }
                (state, Some(report))
            }
            Finish::Failed(why) => {
                self.jl(id, "failed", json!({"reason": why}));
                let _ = self.cancel(id);
                let _ = self.collect_children(id).await;
                (NodeState::Failed(why), None)
            }
            Finish::Cancelled => {
                self.jl(id, "cancelled", json!({}));
                let _ = self.cancel(id);
                let _ = self.collect_children(id).await;
                (NodeState::Cancelled, None)
            }
        };
        let _ = report;
        let (parent, initial, remaining, spent) = self
            .with_node(id, |r| {
                r.state = state.clone();
                (
                    r.parent.clone(),
                    r.ledger.initial,
                    r.ledger.remaining,
                    r.ledger.spent,
                )
            })
            .expect("node");
        self.jl(
            id,
            "done",
            json!({"state": state.label(), "spent": spent, "remaining": remaining}),
        );
        if let Some(p) = parent {
            self.with_node(&p, |pr| pr.ledger.settle(initial, remaining));
            self.jl(&p, "settled", json!({"child": id, "refunded": remaining}));
        }
    }
}
