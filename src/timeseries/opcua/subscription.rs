//! One native subscription epoch, bounded receipt queue and explicit loss retirement.
use super::{
    Mode, Reading, Source, State, connection_error, status_error, task_error, timeout_error,
};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use opcua::{
    client::{
        UARequest as _,
        services::{CreateMonitoredItems, CreateSubscription, Publish, Republish},
    },
    types::{
        DataChangeFilter, DataChangeTrigger, ExtensionObject, MonitoredItemCreateRequest,
        MonitoringMode, MonitoringParameters, SubscriptionAcknowledgement, TimestampsToReturn,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Condvar, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::Notify};

mod message;
#[cfg(test)]
mod tests;
mod value;

const QUEUE_MESSAGES: usize = 8;
const QUEUE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    #[serde(default = "default_signal")]
    pub signal: String,
    #[serde(
        default = "default_publishing",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub publishing_ms: u64,
    #[serde(
        default = "default_queue",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub queue_size: u32,
    #[serde(
        default = "default_notifications",
        deserialize_with = "crate::config_number::unsigned"
    )]
    pub max_notifications_per_publish: u32,
}
fn default_signal() -> String {
    "subscription".into()
}
const fn default_publishing() -> u64 {
    100
}
const fn default_queue() -> u32 {
    128
}
const fn default_notifications() -> u32 {
    1024
}
impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        crate::timeseries::validate_name(&self.signal, "OPC-UA subscription signal")?;
        if !(1..=3_600_000).contains(&self.publishing_ms)
            || !(2..=4096).contains(&self.queue_size)
            || !(1..=1024).contains(&self.max_notifications_per_publish)
        {
            return Err(Usage("OPC-UA subscription publishing_ms must be 1–3600000, queue_size 2–4096 and max_notifications_per_publish 1–1024".into()).into());
        }
        Ok(())
    }
}

pub(super) fn check(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(timeout_error());
    }
    Ok(())
}
fn remaining(deadline: Instant) -> Result<Duration> {
    check(deadline)?;
    Ok(deadline.saturating_duration_since(Instant::now()))
}
pub(super) fn incomplete(message: &'static str) -> anyhow::Error {
    Categorized::error(ErrorKind::Incomplete, message)
}
fn clock() -> Result<u64> {
    crate::host::monotonic_ns().ok_or_else(|| {
        Categorized::error(
            ErrorKind::Unavailable,
            "OPC-UA subscription host monotonic clock is unavailable",
        )
    })
}

struct Failure {
    message: String,
    kind: ErrorKind,
    hint: Option<String>,
    details: Option<Value>,
}
impl Failure {
    fn new(error: &anyhow::Error) -> Self {
        let classified = crate::types::classify(error);
        Self {
            message: error.to_string(),
            kind: classified.kind,
            hint: classified.hint,
            details: classified.details,
        }
    }
    fn error(&self) -> anyhow::Error {
        Categorized {
            kind: self.kind,
            message: self.message.clone(),
            hint: self.hint.clone(),
            details: self.details.clone(),
        }
        .into()
    }
}
struct Received {
    reading: Reading,
    bytes: usize,
    observed: u64,
}
#[derive(Default)]
struct Mailbox {
    queue: VecDeque<Received>,
    bytes: usize,
    pending: Option<Received>,
    error: Option<Failure>,
    ready: bool,
    initial_received: bool,
    stopping: bool,
    stop_ns: Option<u64>,
    end_ns: Option<u64>,
    done: bool,
}
#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    changed: Condvar,
    stop: Notify,
}
impl Shared {
    fn fail(&self, error: &anyhow::Error) {
        if let Ok(mut mailbox) = self.mailbox.lock()
            && mailbox.error.is_none()
        {
            mailbox.error = Some(Failure::new(error));
        }
        self.changed.notify_all();
    }
    fn stopping(&self) -> bool {
        let Ok(mut mailbox) = self.mailbox.lock() else {
            return true;
        };
        if mailbox.stopping {
            return true;
        }
        let Some(end) = mailbox.end_ns else {
            return false;
        };
        match clock() {
            Ok(now) => now >= end,
            Err(error) => {
                if mailbox.error.is_none() {
                    mailbox.error = Some(Failure::new(&error));
                }
                mailbox.stopping = true;
                self.changed.notify_all();
                true
            }
        }
    }
    async fn until_stop(&self) {
        loop {
            if self.stopping() {
                return;
            }
            tokio::select! {
                () = self.stop.notified() => (),
                () = tokio::time::sleep(Duration::from_millis(10)) => (),
            }
        }
    }
    fn stop(&self) {
        if let Ok(mut mailbox) = self.mailbox.lock() {
            mailbox.stopping = true;
            if mailbox.stop_ns.is_none() {
                mailbox.stop_ns = crate::host::monotonic_ns();
            }
        }
        self.stop.notify_one();
        self.changed.notify_all();
    }
    fn pop(mailbox: &mut Mailbox) -> Option<Received> {
        if let Some(received) = mailbox.queue.pop_front() {
            mailbox.bytes -= received.bytes;
            Some(received)
        } else {
            mailbox.pending.take()
        }
    }
    fn within_window(&self, observed: u64) -> Result<bool> {
        let mailbox = self.mailbox.lock().map_err(|_| connection_error())?;
        Ok(mailbox
            .end_ns
            .into_iter()
            .chain(mailbox.stop_ns)
            .min()
            .is_none_or(|cutoff| observed <= cutoff))
    }
    fn failure(&self) -> Result<Option<anyhow::Error>> {
        let mailbox = self.mailbox.lock().map_err(|_| connection_error())?;
        Ok(mailbox.error.as_ref().map(Failure::error))
    }
    async fn admit(
        &self,
        prepared: message::Prepared,
        observed: u64,
        deadline: Instant,
    ) -> Result<bool> {
        let received = Received {
            reading: prepared.reading,
            bytes: prepared.bytes,
            observed,
        };
        loop {
            check(deadline)?;
            {
                let mut mailbox = self.mailbox.lock().map_err(|_| connection_error())?;
                check(deadline)?;
                let cutoff = mailbox.end_ns.into_iter().chain(mailbox.stop_ns).min();
                if cutoff.is_some_and(|cutoff| observed > cutoff) {
                    return Ok(false);
                }
                if mailbox.queue.len() < QUEUE_MESSAGES
                    && received.bytes <= QUEUE_BYTES.saturating_sub(mailbox.bytes)
                {
                    mailbox.bytes += received.bytes;
                    mailbox.queue.push_back(received);
                    mailbox.initial_received = true;
                    if let Some(error) = prepared.terminal {
                        if mailbox.error.is_none() {
                            mailbox.error = Some(Failure::new(&error));
                        }
                        mailbox.stopping = true;
                    }
                    self.changed.notify_all();
                    return Ok(true);
                }
                if mailbox.stopping
                    || mailbox
                        .end_ns
                        .is_some_and(|end| crate::host::monotonic_ns().is_none_or(|now| now >= end))
                {
                    // The sole pending response is owned and pre-cutoff, but was
                    // never FIFO-admitted/ACKed. Finish can retain it for drain.
                    if mailbox.pending.is_some() {
                        return Err(incomplete("OPC-UA pending receipt slot is occupied"));
                    }
                    mailbox.pending = Some(received);
                    mailbox.initial_received = true;
                    if let Some(error) = prepared.terminal
                        && mailbox.error.is_none()
                    {
                        mailbox.error = Some(Failure::new(&error));
                    }
                    self.changed.notify_all();
                    return Ok(false);
                }
            }
            tokio::time::sleep(Duration::from_millis(2).min(remaining(deadline)?)).await;
        }
    }
}

struct Completion(Arc<Shared>);
impl Drop for Completion {
    fn drop(&mut self) {
        if let Ok(mut mailbox) = self.0.mailbox.lock() {
            if std::thread::panicking() && mailbox.error.is_none() {
                mailbox.error = Some(Failure::new(&Categorized::error(
                    ErrorKind::Unavailable,
                    "OPC-UA subscription owner stopped unexpectedly",
                )));
            }
            mailbox.done = true;
        }
        self.0.changed.notify_all();
    }
}
struct Subscription {
    prepared: Option<(Runtime, State)>,
    shared: Arc<Shared>,
    owner: Option<JoinHandle<()>>,
    observed: Option<u64>,
}
pub(super) fn open(runtime: Runtime, state: State) -> Box<dyn Source> {
    Box::new(Subscription {
        prepared: Some((runtime, state)),
        shared: Arc::new(Shared::default()),
        owner: None,
        observed: None,
    })
}
impl Subscription {
    fn start(&mut self, deadline: Instant) -> Result<()> {
        if self.prepared.is_none() {
            return Ok(());
        }
        {
            let mailbox = self.shared.mailbox.lock().map_err(|_| connection_error())?;
            if mailbox.error.is_some() || mailbox.stopping || mailbox.done {
                return Ok(());
            }
        }
        check(deadline)?;
        clock()?;
        let Some((runtime, mut state)) = self.prepared.take() else {
            return Ok(());
        };
        let shared = Arc::clone(&self.shared);
        self.owner = Some(
            std::thread::Builder::new()
                .name("opcua-subscription".into())
                .spawn(move || {
                    let _completion = Completion(Arc::clone(&shared));
                    if let Err(error) = runtime.block_on(run(&mut state, &shared, deadline)) {
                        shared.fail(&error);
                    }
                    state.abort();
                    runtime.shutdown_timeout(Duration::ZERO);
                })
                .map_err(|_| {
                    Categorized::error(
                        ErrorKind::Unavailable,
                        "OPC-UA subscription owner could not start",
                    )
                })?,
        );
        Ok(())
    }
    fn receive(&mut self, deadline: Instant, wait: bool) -> Result<Option<Reading>> {
        let mut mailbox = self.shared.mailbox.lock().map_err(|_| connection_error())?;
        loop {
            if let Some(received) = Shared::pop(&mut mailbox) {
                self.observed = Some(received.observed);
                self.shared.changed.notify_all();
                return Ok(Some(received.reading));
            }
            if let Some(error) = &mailbox.error {
                return Err(error.error());
            }
            if mailbox.done || !wait {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                if !mailbox.initial_received {
                    let error = timeout_error();
                    mailbox.error = Some(Failure::new(&error));
                    mailbox.stopping = true;
                    self.shared.stop.notify_one();
                    return Err(error);
                }
                return Ok(None);
            }
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(mailbox, deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| connection_error())?;
            mailbox = next;
        }
    }
}
impl Source for Subscription {
    fn read(&mut self, timeout: Duration) -> Result<Vec<Reading>> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Categorized::error(ErrorKind::Usage, "OPC-UA timeout is too large"))?;
        if !timeout.is_zero()
            && let Err(error) = self.start(deadline)
        {
            self.shared.fail(&error);
            return Err(error);
        }
        Ok(self
            .receive(deadline, !timeout.is_zero())?
            .into_iter()
            .collect())
    }
    fn ready(&self) -> bool {
        self.shared
            .mailbox
            .lock()
            .is_ok_and(|mailbox| mailbox.ready)
    }
    fn observed_ns(&self) -> Option<u64> {
        self.observed
    }
    fn buffered_timestamps(&self) -> bool {
        true
    }
    fn set_acquisition_end(&mut self, end_ns: Option<u64>) {
        if let Ok(mut mailbox) = self.shared.mailbox.lock() {
            mailbox.end_ns = end_ns;
        }
        self.shared.stop.notify_one();
    }
    fn finish(&mut self, timeout: Duration) -> Result<()> {
        self.shared.stop();
        self.prepared.take();
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(timeout_error)?;
        let mut mailbox = self.shared.mailbox.lock().map_err(|_| connection_error())?;
        while self
            .owner
            .as_ref()
            .is_some_and(|owner| !owner.is_finished())
        {
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(
                    mailbox,
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(2)),
                )
                .map_err(|_| connection_error())?;
            mailbox = next;
        }
        drop(mailbox);
        if let Some(owner) = self.owner.take() {
            check(deadline)?;
            owner.join().map_err(|_| connection_error())?;
        }
        Ok(())
    }
    fn drain(&mut self) -> Result<Option<Reading>> {
        self.receive(Instant::now(), false)
    }
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

async fn call<T>(
    state: &mut State,
    shared: &Shared,
    deadline: Instant,
    future: impl Future<Output = std::result::Result<T, opcua::types::Error>>,
) -> Result<Option<T>> {
    check(deadline)?;
    let task = state.task.as_mut().ok_or_else(connection_error)?;
    tokio::select! {
        biased;
        () = shared.until_stop() => Ok(None),
        () = tokio::time::sleep_until(deadline.into()) => Err(timeout_error()),
        status = task => Err(task_error(&status)),
        result = future => {
            let value = result.map_err(|error| status_error(error.status(),true))?;
            check(deadline)?;
            Ok(Some(value))
        },
    }
}

async fn run(state: &mut State, shared: &Shared, first_deadline: Instant) -> Result<()> {
    let result = run_inner(state, shared, first_deadline).await;
    if let Err(error) = &result {
        shared.fail(error);
    }
    if let Some(session) = state.session.take() {
        session.disable_reconnects();
        let _ = tokio::time::timeout(state.request_timeout, session.disconnect()).await;
    }
    result
}
async fn run_inner(state: &mut State, shared: &Shared, first_deadline: Instant) -> Result<()> {
    if state.config.mode != Mode::Subscription {
        return Err(incomplete("OPC-UA subscription owner mode mismatch"));
    }
    let config = state
        .config
        .subscription
        .clone()
        .ok_or_else(|| incomplete("OPC-UA subscription configuration is absent"))?;
    let connected = tokio::select! {
        biased;
        () = shared.until_stop() => return Ok(()),
        result = tokio::time::timeout_at(first_deadline.into(),state.connect(first_deadline.into())) => result.map_err(|_| timeout_error())?,
    };
    connected?;
    let session = Arc::clone(state.session.as_ref().ok_or_else(connection_error)?);
    session.disable_reconnects();
    let Some(created) = call(
        state,
        shared,
        first_deadline,
        CreateSubscription::new(&session)
            .publishing_interval(Duration::from_millis(config.publishing_ms))
            .max_lifetime_count(30)
            .max_keep_alive_count(1)
            .max_notifications_per_publish(config.max_notifications_per_publish)
            .priority(0)
            .publishing_enabled(true)
            .timeout(remaining(first_deadline)?)
            .send(session.channel()),
    )
    .await?
    else {
        return Ok(());
    };
    validate_subscription(&created, state.request_timeout)?;
    let sampling_ms = f64::from(
        u32::try_from(state.config.sampling_ms)
            .map_err(|_| incomplete("OPC-UA sampling interval exceeds its validated bound"))?,
    );
    let requests = state
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            Ok(MonitoredItemCreateRequest {
                item_to_monitor: node.clone(),
                monitoring_mode: MonitoringMode::Reporting,
                requested_parameters: MonitoringParameters {
                    client_handle: u32::try_from(index + 1)
                        .map_err(|_| incomplete("OPC-UA client handle overflow"))?,
                    sampling_interval: sampling_ms,
                    filter: ExtensionObject::new(DataChangeFilter {
                        trigger: DataChangeTrigger::StatusValueTimestamp,
                        deadband_type: 0,
                        deadband_value: 0.0,
                    }),
                    queue_size: config.queue_size,
                    discard_oldest: false,
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(items) = call(
        state,
        shared,
        first_deadline,
        CreateMonitoredItems::new(created.subscription_id, &session)
            .timestamps_to_return(TimestampsToReturn::Both)
            .items_to_create(requests)
            .timeout(remaining(first_deadline)?)
            .send(session.channel()),
    )
    .await?
    else {
        return Ok(());
    };
    if items.results.len() != state.config.nodes.len() {
        return Err(incomplete("OPC-UA monitored-item result count is invalid"));
    }
    let mut ids = BTreeSet::new();
    let mut nodes = Vec::new();
    let mut revised = Vec::new();
    for (index, item) in items.results.iter().enumerate() {
        check(first_deadline)?;
        let result = &item.result;
        validate_monitored_item(result, index)?;
        if !ids.insert(result.monitored_item_id) {
            return Err(incomplete("OPC-UA monitored-item identifier was reused"));
        }
        let configured = state
            .config
            .nodes
            .get(index)
            .ok_or_else(|| incomplete("OPC-UA configured node count is invalid"))?;
        let resolved = state
            .nodes
            .get(index)
            .ok_or_else(|| incomplete("OPC-UA resolved node count is invalid"))?;
        nodes.push(message::Node {
            name: configured.name.clone(),
            node_id: resolved.node_id.to_string(),
        });
        revised.push(
            json!({"client_handle":index+1,"monitored_item_id":result.monitored_item_id,"status_raw":result.status_code.bits(),
            "sampling_ms":result.revised_sampling_interval,"queue_size":result.revised_queue_size}),
        );
    }
    let setup = json!({"requested":{"publishing_ms":config.publishing_ms,"sampling_ms":state.config.sampling_ms,
        "queue_size":config.queue_size,"max_notifications_per_publish":config.max_notifications_per_publish,
        "timestamps":"both","trigger":"status_value_timestamp","deadband":"none","discard_oldest":false},
        "revised":{"publishing_ms":created.revised_publishing_interval,"lifetime_count":created.revised_lifetime_count,
            "max_keep_alive_count":created.revised_max_keep_alive_count,
            "subscription_status_raw":created.response_header.service_result.bits(),
            "monitored_items_status_raw":items.response_header.service_result.bits(),"items":revised}});
    message::count(&setup, first_deadline)?;
    shared.mailbox.lock().map_err(|_| connection_error())?.ready = true;
    shared.changed.notify_all();
    let mut setup = Some(setup);
    let mut sequence = message::Sequence::default();
    let mut acknowledgements = Vec::new();
    let mut first = true;
    loop {
        if shared.stopping() {
            return Ok(());
        }
        let deadline = if first {
            first_deadline
        } else {
            Instant::now()
                .checked_add(state.request_timeout)
                .ok_or_else(timeout_error)?
        };
        first = false;
        let sent = std::mem::take(&mut acknowledgements);
        let Some(response) = call(
            state,
            shared,
            deadline,
            Publish::new(&session)
                .acks(sent.clone())
                .timeout(remaining(deadline)?)
                .send(session.channel()),
        )
        .await?
        else {
            return Ok(());
        };
        let observed = clock()?;
        if !shared.within_window(observed)? {
            return Ok(());
        }
        let retransmission = message::validate_publish(&response, created.subscription_id, &sent)?;
        let gap = sequence.gap(&response.notification_message)?;
        let repairing = !gap.is_empty();
        let mut frontier = observed;
        if !gap.is_empty() && !retransmission {
            return Err(incomplete(
                "OPC-UA notification gap cannot be republished by this server",
            ));
        }
        for missing in gap {
            let Some(recovered) = call(
                state,
                shared,
                deadline,
                Republish::new(created.subscription_id, missing, &session)
                    .timeout(remaining(deadline)?)
                    .send(session.channel()),
            )
            .await
            .map_err(|error| republish_error(error, missing))?
            else {
                return Err(incomplete(
                    "OPC-UA acquisition stopped before its detected notification gap was repaired",
                ));
            };
            let recovered_observed = clock()?;
            if recovered_observed < frontier {
                return Err(incomplete(
                    "OPC-UA host receive clock moved backwards during recovery",
                ));
            }
            frontier = recovered_observed;
            if recovered.notification_message.sequence_number != missing
                || !message::has_data(&recovered.notification_message)
            {
                return Err(incomplete(
                    "OPC-UA Republish did not return the requested real message",
                ));
            }
            let envelope = message::Envelope {
                config: &config,
                subscription_id: created.subscription_id,
                nodes: &nodes,
                setup: setup.as_ref(),
                response_header: &recovered.response_header,
                publish: None,
            };
            let prepared = envelope.prepare(&recovered.notification_message, deadline)?;
            if !shared.admit(prepared, recovered_observed, deadline).await? {
                return Err(incomplete(
                    "OPC-UA acquisition stopped before its detected notification gap was repaired",
                ));
            }
            sequence.admit(&recovered.notification_message)?;
            if let Some(error) = shared.failure()? {
                return Err(error);
            }
            setup = None;
            acknowledgements.push(SubscriptionAcknowledgement {
                subscription_id: created.subscription_id,
                sequence_number: missing,
            });
        }
        let envelope = message::Envelope {
            config: &config,
            subscription_id: created.subscription_id,
            nodes: &nodes,
            setup: setup.as_ref(),
            response_header: &response.response_header,
            publish: Some((&response, &sent)),
        };
        let prepared = if repairing {
            let pending = envelope.prepare_reserved(
                &response.notification_message,
                deadline,
                message::RECOVERY_RESERVE,
            )?;
            message::recovery_release(pending, observed, frontier, deadline)?
        } else {
            envelope.prepare(&response.notification_message, deadline)?
        };
        if !shared.admit(prepared, frontier, deadline).await? {
            return Ok(());
        }
        sequence.admit(&response.notification_message)?;
        setup = None;
        if retransmission && message::has_data(&response.notification_message) {
            acknowledgements.push(SubscriptionAcknowledgement {
                subscription_id: created.subscription_id,
                sequence_number: response.notification_message.sequence_number,
            });
        }
    }
}

fn republish_error(error: anyhow::Error, sequence: u32) -> anyhow::Error {
    let classified = crate::types::classify(&error);
    if classified
        .details
        .as_ref()
        .and_then(|details| details["opcua_status"].as_u64())
        == Some(u64::from(
            opcua::types::StatusCode::BadMessageNotAvailable.bits(),
        ))
    {
        return Categorized::new(ErrorKind::Incomplete,"OPC-UA missing notification is no longer available")
            .details(json!({"sequence_number":sequence,"opcua_status":opcua::types::StatusCode::BadMessageNotAvailable.bits()})).into();
    }
    error
}

fn validate_subscription(
    created: &opcua::types::CreateSubscriptionResponse,
    timeout: Duration,
) -> Result<()> {
    if created.subscription_id == 0
        || !created.revised_publishing_interval.is_finite()
        || created.revised_publishing_interval <= 0.0
        || created.revised_lifetime_count == 0
        || created.revised_max_keep_alive_count == 0
        || created
            .revised_max_keep_alive_count
            .checked_mul(3)
            .is_none_or(|minimum| created.revised_lifetime_count < minimum)
    {
        return Err(incomplete(
            "OPC-UA server returned invalid subscription revisions",
        ));
    }

    let cadence =
        created.revised_publishing_interval * f64::from(created.revised_max_keep_alive_count);
    if !cadence.is_finite() || cadence + 10.0 >= timeout.as_secs_f64() * 1000.0 {
        return Err(Categorized::new(
            ErrorKind::Unsupported,
            "OPC-UA revised keepalive cadence cannot fit request_timeout_ms",
        )
        .details(
            json!({"revised_publishing_ms":created.revised_publishing_interval,
                "revised_max_keep_alive_count":created.revised_max_keep_alive_count,
                "required_slack_ms":10,
                "request_timeout_ms":timeout.as_millis()}),
        )
        .into());
    }
    Ok(())
}

fn validate_monitored_item(
    result: &opcua::types::MonitoredItemCreateResult,
    index: usize,
) -> Result<()> {
    if !result.status_code.is_good() {
        return Err(Categorized::new(
            ErrorKind::Unsupported,
            "OPC-UA monitored item or full-fidelity filter was refused",
        )
        .details(json!({"node_index":index,"opcua_status":result.status_code.bits()}))
        .into());
    }
    if result.monitored_item_id == 0
        || !result.revised_sampling_interval.is_finite()
        || result.revised_sampling_interval < 0.0
        || result.revised_queue_size < 2
        || result.filter_result.body.is_some()
    {
        return Err(incomplete(
            "OPC-UA monitored-item revisions cannot provide the configured capture contract",
        ));
    }
    Ok(())
}
