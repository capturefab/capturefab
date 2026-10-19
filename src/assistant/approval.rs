//! Human decisions are bound to checked actions. A grant for a task cannot
//! silently add a new camera, risk class or egress target.
use crate::{
    session::{ActivityOrigin, ActorKind, SessionActor},
    tools::{Checked, Op, Risk, redact},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::{Duration, Instant},
};

pub const DENIAL_HINT: &str = "Do not retry this action. Continue another way or call finish_task.";
pub const WAIT_SLICE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    Supervised,
    Autonomous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    Once,
    #[serde(rename = "run", alias = "for_run")]
    ForRun,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovalRequest {
    pub id: u64,
    pub run: String,
    pub tool: String,
    pub risk: Risk,
    pub title: String,
    pub summary: String,
    pub facts: Vec<(String, String)>,
    pub reasons: Vec<String>,
    pub camera: Option<String>,
    pub needs_secret: bool,
    pub allow_run_scope: bool,
    pub expires_at_ms: u64,
}
impl ApprovalRequest {
    pub fn new(id: u64, run: &str, checked: &Checked, expires_at_ms: u64) -> Self {
        Self {
            id,
            run: redact::for_output_text(run),
            tool: checked.call.tool.clone(),
            risk: checked.risk,
            title: redact::for_output_text(&checked.title),
            summary: redact::for_output_text(&checked.summary),
            facts: checked
                .facts
                .iter()
                .map(|(key, value)| (redact::for_output_text(key), redact::for_output_text(value)))
                .collect(),
            reasons: checked
                .reasons
                .iter()
                .map(|reason| redact::for_output_text(reason))
                .collect(),
            camera: checked.call.args["camera"]
                .as_str()
                .map(redact::for_output_text),
            needs_secret: checked.ops.iter().any(|op| {
                matches!(
                    op,
                    Op::CreateDestination {
                        needs_secret: true,
                        ..
                    }
                )
            }),
            allow_run_scope: checked.risk != Risk::HumanOnly,
            expires_at_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approved {
        scope: ApprovalScope,
        by: SessionActor,
    },
    Denied {
        note: Option<String>,
        by: SessionActor,
    },
    #[serde(rename = "timeout")]
    TimedOut,
    Cancelled,
}

pub trait Approver: Send + Sync {
    fn request(
        &self,
        req: ApprovalRequest,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> ApprovalDecision;
}

pub struct DenyApprover {
    pub by: SessionActor,
}
impl DenyApprover {
    pub fn new(by: SessionActor) -> Self {
        Self { by }
    }
}
impl Default for DenyApprover {
    fn default() -> Self {
        Self::new(SessionActor::process(ActivityOrigin::Client))
    }
}
impl Approver for DenyApprover {
    fn request(
        &self,
        _req: ApprovalRequest,
        cancel: &AtomicBool,
        _timeout: Duration,
    ) -> ApprovalDecision {
        if cancel.load(Ordering::Relaxed) {
            ApprovalDecision::Cancelled
        } else {
            ApprovalDecision::Denied {
                note: None,
                by: self.by.clone(),
            }
        }
    }
}

type Pending = BTreeMap<(String, u64), Sender<ApprovalDecision>>;

/// The worker waits here while a GUI or another local front end reads the
/// request queue. Late decisions are discarded when the request expires.
pub struct ChannelApprover {
    requests: Sender<ApprovalRequest>,
    pending: Mutex<Pending>,
}
impl ChannelApprover {
    pub fn new() -> (Arc<Self>, Receiver<ApprovalRequest>) {
        let (requests, receiver) = mpsc::channel();
        (
            Arc::new(Self {
                requests,
                pending: Mutex::new(BTreeMap::new()),
            }),
            receiver,
        )
    }
    pub fn respond(&self, run: &str, id: u64, decision: ApprovalDecision) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(run.into(), id))
            .is_some_and(|sender| sender.send(decision).is_ok())
    }
}
impl Approver for ChannelApprover {
    fn request(
        &self,
        req: ApprovalRequest,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> ApprovalDecision {
        if cancel.load(Ordering::Relaxed) {
            return ApprovalDecision::Cancelled;
        }
        let key = (req.run.clone(), req.id);
        let (sender, receiver) = mpsc::channel();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if pending.contains_key(&key) {
                return ApprovalDecision::TimedOut;
            }
            pending.insert(key.clone(), sender);
        }
        let decision = if self.requests.send(req).is_err() {
            ApprovalDecision::TimedOut
        } else {
            wait_response(&receiver, cancel, timeout, || true)
        };
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&key);
        decision
    }
}

/// The hook runs at least every 250 ms, allowing a caller to enforce a wall
/// deadline or detect a human change while waiting for a decision.
pub fn wait_response(
    receiver: &Receiver<ApprovalDecision>,
    cancel: &AtomicBool,
    timeout: Duration,
    mut on_slice: impl FnMut() -> bool,
) -> ApprovalDecision {
    let start = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return ApprovalDecision::Cancelled;
        }
        if !on_slice() {
            return ApprovalDecision::Cancelled;
        }
        let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
            return ApprovalDecision::TimedOut;
        };
        if remaining.is_zero() {
            return ApprovalDecision::TimedOut;
        }
        match receiver.recv_timeout(remaining.min(WAIT_SLICE)) {
            Ok(decision) => {
                if cancel.load(Ordering::Relaxed) || !on_slice() {
                    return ApprovalDecision::Cancelled;
                }
                if start.elapsed() >= timeout {
                    return ApprovalDecision::TimedOut;
                }
                return decision;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return ApprovalDecision::TimedOut,
        }
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GrantKey {
    tool: String,
    camera: Option<String>,
    risk: Risk,
    egress: Option<[u8; 32]>,
}
fn grant_key(checked: &Checked) -> GrantKey {
    let egress = checked.leaves_computer.then(|| {
        let destination = checked.destination.as_ref().map(|destination| {
            json!({
                "name":destination.name,"kind":destination.kind,"location":destination.location,
                "host":destination.host,"remote":destination.remote,"insecure":destination.insecure,
            })
        });
        Sha256::digest(
            serde_json::to_vec(&json!({"url":checked.call.args["url"],"destination":destination}))
                .expect("JSON egress binding"),
        )
        .into()
    });
    GrantKey {
        tool: checked.call.tool.clone(),
        camera: checked.call.args["camera"].as_str().map(str::to_owned),
        risk: checked.risk,
        egress,
    }
}

pub struct ApprovalPolicy {
    mode: RunMode,
    grants: BTreeMap<GrantKey, SessionActor>,
    denials: u32,
}
impl ApprovalPolicy {
    pub fn new(mode: RunMode) -> Self {
        Self {
            mode,
            grants: BTreeMap::new(),
            denials: 0,
        }
    }
    pub fn granted(&self, checked: &Checked) -> Option<SessionActor> {
        (checked.risk != Risk::HumanOnly)
            .then(|| self.grants.get(&grant_key(checked)).cloned())
            .flatten()
    }
    pub fn requires(&self, checked: &Checked) -> bool {
        if checked.risk == Risk::Observe || self.granted(checked).is_some() {
            return false;
        }
        self.mode == RunMode::Supervised
            || matches!(checked.risk, Risk::Disruptive | Risk::HumanOnly)
    }
    /// Return the actual scope used in the log. Human-only actions can be
    /// approved once even if a front end mistakenly offers a task grant.
    pub fn record(&mut self, checked: &Checked, decision: &ApprovalDecision) -> ApprovalDecision {
        let decision = match decision {
            ApprovalDecision::Approved { by, .. }
                if by.kind != ActorKind::Human || by.validate().is_err() =>
            {
                ApprovalDecision::Denied {
                    note: Some("A person must approve this action.".into()),
                    by: by.clone(),
                }
            }
            ApprovalDecision::Approved { scope, by } => {
                let scope = if checked.risk == Risk::HumanOnly {
                    ApprovalScope::Once
                } else {
                    *scope
                };
                if scope == ApprovalScope::ForRun {
                    self.grants.insert(grant_key(checked), by.clone());
                }
                ApprovalDecision::Approved {
                    scope,
                    by: by.clone(),
                }
            }
            other => other.clone(),
        };
        if matches!(
            decision,
            ApprovalDecision::Denied { .. } | ApprovalDecision::TimedOut
        ) {
            self.denials = self.denials.saturating_add(1);
        }
        decision
    }
    pub fn denials(&self) -> u32 {
        self.denials
    }
    /// A fresh human write supersedes task grants on that camera, including
    /// an earlier grant that was already in the disruptive class.
    pub fn invalidate_camera(&mut self, camera: &str) {
        self.grants
            .retain(|key, _| key.camera.as_deref() != Some(camera));
    }
    pub fn needs_user(&self) -> bool {
        self.denials >= 3
    }
}

#[derive(Debug)]
pub struct ApprovalChanged;
impl fmt::Display for ApprovalChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The action changed while waiting for approval. Review it again.")
    }
}
impl std::error::Error for ApprovalChanged {}

/// Fresh checking happens in the runtime. This final binding check permits
/// no altered arguments, camera, destination, command, risk or output root.
pub fn recheck_approved(approved: &Checked, fresh: &Checked) -> Result<(), ApprovalChanged> {
    let destination = |checked: &Checked| {
        checked.destination.as_ref().map(|destination| {
            (
                destination.name.clone(),
                destination.kind.clone(),
                destination.location.clone(),
                destination.host.clone(),
                destination.remote,
                destination.insecure,
            )
        })
    };
    if !fresh.ready()
        || approved.id != fresh.id
        || approved.call != fresh.call
        || approved.risk != fresh.risk
        || approved.title != fresh.title
        || approved.summary != fresh.summary
        || crate::tools::exec::approval_facts(approved) != crate::tools::exec::approval_facts(fresh)
        || approved.reasons != fresh.reasons
        || approved.leaves_computer != fresh.leaves_computer
        || approved.output_root != fresh.output_root
        || destination(approved) != destination(fresh)
        || approved.issues != fresh.issues
        || !crate::tools::exec::same_approval_operations(approved, fresh)
    {
        Err(ApprovalChanged)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        session::SessionCommand,
        tools::{CameraRef, Issue, StepId, ToolCall, context::DestinationInfo},
    };
    use serde_json::Value;
    use std::thread;

    fn human() -> SessionActor {
        SessionActor::new("desktop:1", "Desktop", ActorKind::Human).unwrap()
    }
    fn checked(risk: Risk) -> Checked {
        let mut checked = Checked::new(
            StepId(1),
            &ToolCall {
                tool: "set_camera_features".into(),
                args: json!({"camera":"sim:0","changes":[{"feature":"Gain","value":"2"}]}),
            },
        );
        checked.risk = risk;
        checked.ops.push(Op::Command {
            camera: CameraRef::Connected("sim:0".into()),
            command: SessionCommand::Set {
                feature: "Gain".into(),
                value: "2".into(),
            },
        });
        checked
    }
    fn approved(scope: ApprovalScope) -> ApprovalDecision {
        ApprovalDecision::Approved { scope, by: human() }
    }

    #[test]
    fn the_complete_mode_and_risk_table_is_enforced() {
        for mode in [RunMode::Supervised, RunMode::Autonomous] {
            for risk in [
                Risk::Observe,
                Risk::Reversible,
                Risk::Bounded,
                Risk::Disruptive,
                Risk::HumanOnly,
            ] {
                let expected = risk != Risk::Observe
                    && (mode == RunMode::Supervised || risk >= Risk::Disruptive);
                assert_eq!(
                    ApprovalPolicy::new(mode).requires(&checked(risk)),
                    expected,
                    "{mode:?} {risk:?}"
                );
            }
        }
    }

    #[test]
    fn task_grants_bind_tool_camera_risk_and_exact_egress() {
        let mut policy = ApprovalPolicy::new(RunMode::Supervised);
        let original = checked(Risk::Reversible);
        policy.record(&original, &approved(ApprovalScope::Once));
        assert!(policy.requires(&original));
        policy.record(&original, &approved(ApprovalScope::ForRun));
        assert!(!policy.requires(&original));
        for index in 0..3 {
            let mut changed = original.clone();
            match index {
                0 => changed.call.tool = "set_camera_roi".into(),
                1 => changed.call.args["camera"] = json!("sim:1"),
                _ => changed.risk = Risk::Disruptive,
            }
            assert!(policy.requires(&changed));
        }
        policy.invalidate_camera("sim:0");
        assert!(policy.requires(&original));
        let mut stream = checked(Risk::Disruptive);
        stream.call.tool = "stream_to_url".into();
        stream.call.args["url"] = json!("rtsp://one.example/live");
        stream.leaves_computer = true;
        policy.record(&stream, &approved(ApprovalScope::ForRun));
        assert!(!policy.requires(&stream));
        for url in ["rtsp://two.example/live", "rtsp://one.example/another"] {
            let mut changed = stream.clone();
            changed.call.args["url"] = json!(url);
            assert!(policy.requires(&changed));
        }
        let mut upload = checked(Risk::Bounded);
        upload.leaves_computer = true;
        upload.destination = Some(DestinationInfo {
            name: "captures".into(),
            kind: "s3".into(),
            location: "one-bucket".into(),
            host: Some("s3.example".into()),
            insecure: false,
            problem: None,
            free_bytes: Some(100),
            remote: true,
        });
        policy.record(&upload, &approved(ApprovalScope::ForRun));
        let mut changed = upload.clone();
        changed.destination.as_mut().unwrap().free_bytes = Some(5);
        assert!(!policy.requires(&changed));
        changed.destination.as_mut().unwrap().location = "two-bucket".into();
        assert!(policy.requires(&changed));
    }

    #[test]
    fn human_only_never_gets_a_task_grant_and_agents_cannot_approve() {
        let action = checked(Risk::HumanOnly);
        let mut policy = ApprovalPolicy::new(RunMode::Autonomous);
        let actual = policy.record(&action, &approved(ApprovalScope::ForRun));
        assert!(matches!(
            actual,
            ApprovalDecision::Approved {
                scope: ApprovalScope::Once,
                ..
            }
        ));
        assert!(policy.requires(&action));
        assert!(policy.granted(&action).is_none());
        let agent = SessionActor::new("assistant:1", "Assistant", ActorKind::Agent).unwrap();
        assert!(matches!(
            policy.record(
                &action,
                &ApprovalDecision::Approved {
                    scope: ApprovalScope::ForRun,
                    by: agent
                }
            ),
            ApprovalDecision::Denied { .. }
        ));
        assert_eq!(policy.denials(), 1);
        policy.record(&action, &ApprovalDecision::TimedOut);
        policy.record(
            &action,
            &ApprovalDecision::Denied {
                note: None,
                by: human(),
            },
        );
        assert!(policy.needs_user());
        policy.record(&action, &ApprovalDecision::Cancelled);
        assert_eq!(policy.denials(), 3);
        assert_eq!(
            serde_json::to_value(ApprovalScope::ForRun).unwrap(),
            json!("run")
        );
    }

    #[test]
    fn approval_cards_use_redacted_checked_facts() {
        let mut action = checked(Risk::Disruptive);
        action.summary = "Send to rtsp://alice:planted-password@camera/live".into();
        action.facts.push((
            "URL".into(),
            "srt://camera?passphrase=planted-passphrase".into(),
        ));
        let request = ApprovalRequest::new(7, "r-test", &action, 1000);
        let text = format!("{request:?} {}", serde_json::to_string(&request).unwrap());
        assert!(!text.contains("planted-password"));
        assert!(!text.contains("planted-passphrase"));
        assert!(request.allow_run_scope);
        assert_eq!(request.id, 7);
    }

    #[test]
    fn channel_wait_supports_approval_timeout_cancel_and_ignores_late_answers() {
        let (approver, requests) = ChannelApprover::new();
        let worker = approver.clone();
        let handle = thread::spawn(move || {
            worker.request(
                ApprovalRequest::new(1, "r-test", &checked(Risk::Disruptive), 1000),
                &AtomicBool::new(false),
                Duration::from_secs(2),
            )
        });
        let request = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(approver.respond(&request.run, request.id, approved(ApprovalScope::Once)));
        assert!(matches!(
            handle.join().unwrap(),
            ApprovalDecision::Approved { .. }
        ));
        assert!(!approver.respond("r-test", 1, approved(ApprovalScope::Once)));
        assert!(matches!(
            approver.request(
                ApprovalRequest::new(2, "r-test", &checked(Risk::Disruptive), 1000),
                &AtomicBool::new(false),
                Duration::ZERO
            ),
            ApprovalDecision::TimedOut
        ));
        assert!(!approver.respond("r-test", 2, approved(ApprovalScope::Once)));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let worker = approver.clone();
        let handle = thread::spawn(move || {
            worker.request(
                ApprovalRequest::new(3, "r-test", &checked(Risk::Disruptive), 1000),
                &worker_cancel,
                Duration::from_secs(2),
            )
        });
        let _ = requests.recv_timeout(Duration::from_secs(1)).unwrap(); // expired request 2
        let _ = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        cancel.store(true, Ordering::Relaxed);
        assert!(matches!(
            handle.join().unwrap(),
            ApprovalDecision::Cancelled
        ));
        let (_sender, receiver) = mpsc::channel();
        let mut ticks = 0;
        assert!(matches!(
            wait_response(
                &receiver,
                &AtomicBool::new(false),
                Duration::from_secs(1),
                || {
                    ticks += 1;
                    false
                }
            ),
            ApprovalDecision::Cancelled
        ));
        assert_eq!(ticks, 1);
        let (sender, receiver) = mpsc::channel();
        sender.send(approved(ApprovalScope::Once)).unwrap();
        let mut ticks = 0;
        assert!(matches!(
            wait_response(
                &receiver,
                &AtomicBool::new(false),
                Duration::from_millis(1),
                || {
                    ticks += 1;
                    if ticks == 2 {
                        thread::sleep(Duration::from_millis(2));
                    }
                    true
                }
            ),
            ApprovalDecision::TimedOut
        ));
    }

    #[test]
    fn fresh_checks_must_match_the_approved_action_and_still_be_ready() {
        let original = checked(Risk::Reversible);
        assert!(recheck_approved(&original, &original).is_ok());
        for index in 0..10 {
            let mut changed = original.clone();
            match index {
                0 => changed.call.args["camera"] = json!("sim:1"),
                1 => changed.call.args["changes"][0]["value"] = json!("3"),
                2 => changed.risk = Risk::Disruptive,
                3 => changed.output_root = Some("/tmp/another".into()),
                4 => changed.ops.clear(),
                5 => changed.issue(Issue::usage(Some("camera"), "Connect the camera first.")),
                6 => changed.title.push_str(" changed"),
                7 => changed.summary.push_str(" changed"),
                8 => changed.facts.push((
                    "Gain".into(),
                    "2 → 3 (a person changed the previous value)".into(),
                )),
                _ => changed
                    .reasons
                    .push("A person changed this value during the wait.".into()),
            }
            assert!(recheck_approved(&original, &changed).is_err());
        }
        let mut original = checked(Risk::Disruptive);
        original.call.args["changes"][0]["expect"] = Value::Null;
        original.facts = vec![("Gain".into(), "1 → 2".into())];
        let mut fresh = original.clone();
        fresh.facts[0].1 = "3 → 2".into();
        assert!(recheck_approved(&original, &fresh).is_err());
    }

    #[test]
    fn elapsed_time_does_not_revoke_a_capture_or_immediate_schedule_approval() {
        use crate::tools::{
            PlanState,
            tests::{fixture_context, fixture_snapshot},
        };
        let before = fixture_context(fixture_snapshot());
        let mut after = before.clone();
        after.now_ms += 61_000;
        after.storage.allocated_bytes += 1024 * 1024;
        for tool in ["capture_photos", "schedule_captures"] {
            let schema = (crate::tools::spec(tool).unwrap().schema)();
            let mut args = Value::Object(
                schema["properties"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(|name| (name.clone(), Value::Null))
                    .collect(),
            );
            args["camera"] = json!("sim:0");
            args["file_name"] = json!("capture-{datetime}.png");
            if tool == "schedule_captures" {
                args["every"] = json!("1m");
                args["count"] = json!(3);
            }
            let call = ToolCall {
                tool: tool.into(),
                args,
            };
            let approved = crate::tools::check(
                &before,
                &mut PlanState::new(&before),
                StepId(1),
                &call,
                &|_, _| false,
            );
            let fresh = crate::tools::check(
                &after,
                &mut PlanState::new(&after),
                StepId(1),
                &approved.call,
                &|_, _| false,
            );
            assert!(approved.ready(), "{approved:?}");
            assert!(fresh.ready(), "{fresh:?}");
            assert_ne!(approved.facts, fresh.facts);
            assert!(recheck_approved(&approved, &fresh).is_ok(), "{tool}");
            let mut redirected = fresh.clone();
            redirected.output_root = Some("/different".into());
            assert!(recheck_approved(&approved, &redirected).is_err());
            let mut changed_size = fresh.clone();
            changed_size
                .facts
                .iter_mut()
                .find(|(name, _)| name == "Storage")
                .unwrap()
                .1 = "≈ 999 GiB · enough free".into();
            assert!(recheck_approved(&approved, &changed_size).is_err());
            if tool == "schedule_captures" {
                let mut changed = fresh.clone();
                for op in &mut changed.ops {
                    if let Op::Command {
                        command: SessionCommand::Schedule { interval_ms, .. },
                        ..
                    } = op
                    {
                        *interval_ms += 1000;
                    }
                }
                assert!(recheck_approved(&approved, &changed).is_err());
            }
        }
    }
}
