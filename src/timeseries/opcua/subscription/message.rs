//! Complete native notification envelopes and one-subscription sequence admission.
use super::{Config, check, incomplete, value};
use crate::timeseries::Reading;
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use opcua::types::{
    DataChangeNotification, NotificationMessage, PublishResponse, ResponseHeader,
    StatusChangeNotification, SubscriptionAcknowledgement,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Write, time::Instant};

pub(super) const MAX_JSON: usize = super::super::MAX_MESSAGE - 16 * 1024;
const MAX_AVAILABLE: usize = 4096;
const MAX_BLOCKS: usize = 64;
pub(super) const MAX_GAP: usize = 64;
pub(super) const RECOVERY_RESERVE: usize = 512;
#[cfg(test)]
mod tests;

pub(super) struct Node {
    pub name: String,
    pub node_id: String,
}

#[derive(Default)]
pub(super) struct Sequence {
    last: u32,
}
impl Sequence {
    const fn next(&self) -> u32 {
        if self.last == u32::MAX {
            1
        } else {
            self.last + 1
        }
    }
    pub(super) fn gap(&self, message: &NotificationMessage) -> Result<Vec<u32>> {
        let target = message.sequence_number;
        if target == 0 {
            return Err(incomplete("OPC-UA notification sequence zero is invalid"));
        }
        let mut next = self.next();
        let mut missing = Vec::new();
        while next != target {
            if missing.len() == MAX_GAP {
                return Err(incomplete("OPC-UA notification gap exceeds 64 messages"));
            }
            missing.push(next);
            next = if next == u32::MAX { 1 } else { next + 1 };
        }
        Ok(missing)
    }
    pub(super) fn admit(&mut self, message: &NotificationMessage) -> Result<()> {
        if message.sequence_number != self.next() {
            return Err(incomplete("OPC-UA notification is not the next message"));
        }
        if has_data(message) {
            self.last = message.sequence_number;
        }
        Ok(())
    }
}
pub(super) fn has_data(message: &NotificationMessage) -> bool {
    message
        .notification_data
        .as_ref()
        .is_some_and(|items| !items.is_empty())
}

pub(super) fn validate_publish(
    response: &PublishResponse,
    subscription: u32,
    sent: &[SubscriptionAcknowledgement],
) -> Result<bool> {
    if response.subscription_id != subscription {
        return Err(incomplete(
            "OPC-UA Publish returned a different subscription",
        ));
    }
    let results = response.results.as_deref().unwrap_or_default();
    if results.len() != sent.len() {
        return Err(incomplete(
            "OPC-UA Publish acknowledgement count does not match",
        ));
    }
    for (ack, status) in sent.iter().zip(results) {
        if !status.is_good() {
            return Err(Categorized::new(
                ErrorKind::Incomplete,
                "OPC-UA server rejected a notification receipt acknowledgement",
            )
            .details(json!({"subscription_id":ack.subscription_id,
                    "sequence_number":ack.sequence_number,"opcua_status":status.bits()}))
            .into());
        }
    }
    let diagnostics = response.diagnostic_infos.as_deref().unwrap_or_default();
    if !diagnostics.is_empty() && diagnostics.len() != sent.len() {
        return Err(incomplete(
            "OPC-UA Publish acknowledgement diagnostics do not match",
        ));
    }
    let available = response
        .available_sequence_numbers
        .as_deref()
        .unwrap_or_default();
    if available.len() > MAX_AVAILABLE {
        return Err(incomplete(
            "OPC-UA available sequence list exceeds its bound",
        ));
    }
    let mut seen = BTreeSet::new();
    if available
        .iter()
        .any(|number| *number == 0 || !seen.insert(*number))
    {
        return Err(incomplete("OPC-UA available sequence list is invalid"));
    }
    if !available.is_empty()
        && has_data(&response.notification_message)
        && !seen.contains(&response.notification_message.sequence_number)
    {
        return Err(incomplete(
            "OPC-UA available sequence list omits its current message",
        ));
    }
    Ok(!available.is_empty())
}

#[derive(Debug)]
pub(super) struct Prepared {
    pub reading: Reading,
    pub bytes: usize,
    pub terminal: Option<anyhow::Error>,
}

pub(super) struct Envelope<'a> {
    pub config: &'a Config,
    pub subscription_id: u32,
    pub nodes: &'a [Node],
    pub setup: Option<&'a Value>,
    pub response_header: &'a ResponseHeader,
    pub publish: Option<(&'a PublishResponse, &'a [SubscriptionAcknowledgement])>,
}
impl Envelope<'_> {
    pub(super) fn prepare(
        &self,
        message: &NotificationMessage,
        deadline: Instant,
    ) -> Result<Prepared> {
        self.prepare_reserved(message, deadline, 0)
    }
    pub(super) fn prepare_reserved(
        &self,
        message: &NotificationMessage,
        deadline: Instant,
        reserve: usize,
    ) -> Result<Prepared> {
        check(deadline)?;
        let limit = MAX_JSON
            .checked_sub(reserve)
            .ok_or_else(|| incomplete("OPC-UA envelope reserve exceeds its budget"))?;
        let mut out = json!({"kind":"opcua_publish","subscription_id":self.subscription_id,
            "message_type":if has_data(message) {"notification"} else {"keepalive"},
            "delivery":if self.publish.is_some() {"publish"} else {"republish"},
            "sequence_number":message.sequence_number,"publish_time_ticks":message.publish_time.ticks(),
            "lossy":false,"notifications":[]});
        let mut budget = Budget::new(&out, deadline, limit)?;
        if let Some(setup) = self.setup {
            budget.add(count(setup, deadline)? + 9)?;
            out["setup"].clone_from(setup);
        }
        {
            let header = self.response_header;
            let strings = header.string_table.as_deref().unwrap_or_default();
            if strings.len() > MAX_AVAILABLE {
                return Err(incomplete("OPC-UA response string table exceeds its bound"));
            }
            let mut response_header = json!({"timestamp_ticks":header.timestamp.ticks(),
                "request_handle":header.request_handle,"service_status_raw":header.service_result.bits(),
                "string_table_present":header.string_table.is_some(),"string_table":[]});
            budget.add(count(&response_header, deadline)? + 19)?;
            let mut table = Vec::new();
            for string in strings {
                budget.add(count(&string.value(), deadline)? + usize::from(!table.is_empty()))?;
                table.push(
                    string
                        .value()
                        .as_ref()
                        .map_or(Value::Null, |string| Value::String(string.clone())),
                );
            }
            response_header["string_table"] = table.into();
            let diagnostic = value::diagnostic_info(&header.service_diagnostics, deadline)?;
            budget.add(count(&diagnostic, deadline)? + 23)?;
            response_header["service_diagnostics"] = diagnostic;
            let additional = value::extension_object(&header.additional_header, deadline)?;
            budget.add(count(&additional, deadline)? + 21)?;
            response_header["additional_header"] = additional;
            out["response_header"] = response_header;
        }
        if let Some((response, sent)) = self.publish {
            let available = response
                .available_sequence_numbers
                .as_deref()
                .unwrap_or_default();
            let acknowledgements: Vec<Value> = sent
                .iter()
                .zip(response.results.as_deref().unwrap_or_default())
                .map(|(ack, status)| {
                    json!({"subscription_id":ack.subscription_id,
                    "sequence_number":ack.sequence_number,"status_raw":status.bits()})
                })
                .collect();
            let metadata = json!({"more_notifications":response.more_notifications,
                "available_sequence_numbers":available,"acknowledgements":acknowledgements,
                "ack_diagnostic_infos":[]});
            budget.add(count(&metadata, deadline)? - 2 + 1)?;
            for (key, item) in metadata
                .as_object()
                .ok_or_else(|| incomplete("OPC-UA metadata is invalid"))?
            {
                out[key].clone_from(item);
            }
            let mut diagnostics = Vec::new();
            for diagnostic in response.diagnostic_infos.iter().flatten() {
                let diagnostic = value::diagnostic_info(diagnostic, deadline)?;
                budget.add(count(&diagnostic, deadline)? + usize::from(!diagnostics.is_empty()))?;
                diagnostics.push(diagnostic);
            }
            out["ack_diagnostic_infos"] = diagnostics.into();
        }
        let data = message.notification_data.as_deref().unwrap_or_default();
        if data.len() > MAX_BLOCKS {
            return Err(incomplete("OPC-UA notification block count exceeds 64"));
        }
        let mut notifications = Vec::new();
        let mut total = 0_usize;
        let mut terminal = None;
        for object in data {
            check(deadline)?;
            if let Some(change) = object.inner_as::<DataChangeNotification>() {
                let items = change.monitored_items.as_deref().unwrap_or_default();
                total = total
                    .checked_add(items.len())
                    .ok_or_else(|| incomplete("OPC-UA notification count overflow"))?;
                if total
                    > usize::try_from(self.config.max_notifications_per_publish)
                        .map_err(|_| incomplete("OPC-UA notification limit is invalid"))?
                {
                    return Err(incomplete(
                        "OPC-UA server exceeded max_notifications_per_publish",
                    ));
                }
                let diagnostics = change.diagnostic_infos.as_deref().unwrap_or_default();
                if !diagnostics.is_empty() && diagnostics.len() != items.len() {
                    return Err(incomplete("OPC-UA data change diagnostics do not match"));
                }
                let mut block = json!({"kind":"data_change","items":[],"diagnostic_infos":[]});
                budget.add(count(&block, deadline)? + usize::from(!notifications.is_empty()))?;
                let mut converted = Vec::new();
                for item in items {
                    check(deadline)?;
                    let index = item
                        .client_handle
                        .checked_sub(1)
                        .and_then(|index| usize::try_from(index).ok())
                        .ok_or_else(|| incomplete("OPC-UA unknown monitored-item client handle"))?;
                    let node = self
                        .nodes
                        .get(index)
                        .ok_or_else(|| incomplete("OPC-UA unknown monitored-item client handle"))?;
                    let mut native = json!({"client_handle":item.client_handle,"node_index":index,
                        "name":node.name,"node_id":node.node_id});
                    budget
                        .add(count(&native, deadline)? + 14 + usize::from(!converted.is_empty()))?;
                    let data_value = value::data_value(&item.value, deadline)?;
                    budget.add(count(&data_value, deadline)?)?;
                    native["data_value"] = data_value;
                    converted.push(native);
                    if item.value.status().info_type()
                        == opcua::types::StatusCodeInfoType::DataValue
                        && item.value.status().overflow()
                        && terminal.is_none()
                    {
                        terminal = Some(
                            Categorized::new(
                                ErrorKind::Incomplete,
                                "OPC-UA monitored-item queue overflowed; continuity was lost",
                            )
                            .details(json!({"client_handle":item.client_handle,
                                "opcua_status":item.value.status().bits()}))
                            .into(),
                        );
                    }
                }
                block["items"] = converted.into();
                let mut converted = Vec::new();
                for diagnostic in diagnostics {
                    let diagnostic = value::diagnostic_info(diagnostic, deadline)?;
                    budget
                        .add(count(&diagnostic, deadline)? + usize::from(!converted.is_empty()))?;
                    converted.push(diagnostic);
                }
                block["diagnostic_infos"] = converted.into();
                notifications.push(block);
            } else if let Some(status) = object.inner_as::<StatusChangeNotification>() {
                let mut block = json!({"kind":"status_change","status_raw":status.status.bits()});
                budget
                    .add(count(&block, deadline)? + 19 + usize::from(!notifications.is_empty()))?;
                let diagnostic = value::diagnostic_info(&status.diagnostic_info, deadline)?;
                budget.add(count(&diagnostic, deadline)?)?;
                block["diagnostic_info"] = diagnostic;
                notifications.push(block);
                if terminal.is_none() {
                    terminal = Some(
                        Categorized::new(
                            ErrorKind::Unavailable,
                            "OPC-UA subscription reported a terminal status change",
                        )
                        .details(json!({"opcua_status":status.status.bits()}))
                        .into(),
                    );
                }
            } else {
                return Err(Categorized::error(
                    ErrorKind::Unsupported,
                    "OPC-UA Value subscription returned an unsupported notification type",
                ));
            }
        }
        out["notifications"] = notifications.into();
        out["lossy"] = terminal
            .as_ref()
            .is_some_and(|error| crate::types::classify(error).kind == ErrorKind::Incomplete)
            .into();
        let reading = Reading {
            signal: self.config.signal.clone(),
            value: out,
            source_time: None,
        };
        let bytes = count(&[&reading], deadline)?;
        if bytes > limit {
            return Err(incomplete(
                "OPC-UA subscription envelope exceeds its reserved JSON budget",
            ));
        }
        check(deadline)?;
        Ok(Prepared {
            reading,
            bytes,
            terminal,
        })
    }
}

struct Budget {
    bytes: usize,
    limit: usize,
}
impl Budget {
    fn new(value: &Value, deadline: Instant, limit: usize) -> Result<Self> {
        let mut budget = Self { bytes: 0, limit };
        budget.add(count(value, deadline)?)?;
        Ok(budget)
    }
    fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|size| *size <= self.limit)
            .ok_or_else(|| incomplete("OPC-UA subscription envelope exceeds its JSON budget"))?;
        Ok(())
    }
}

pub(super) fn recovery_release(
    mut prepared: Prepared,
    original: u64,
    frontier: u64,
    deadline: Instant,
) -> Result<Prepared> {
    check(deadline)?;
    if frontier < original {
        return Err(incomplete(
            "OPC-UA recovery receive frontier precedes the pending receipt",
        ));
    }
    let pending = std::mem::take(&mut prepared.reading.value);
    prepared.reading.value = Value::Object(
        [
            (
                "kind".into(),
                Value::String("opcua_recovery_release".into()),
            ),
            (
                "clock_basis".into(),
                Value::String("recovery_completed_receive".into()),
            ),
            ("original_received_monotonic_ns".into(), original.into()),
            (
                "recovery_completed_received_monotonic_ns".into(),
                frontier.into(),
            ),
            ("pending".into(), pending),
        ]
        .into_iter()
        .collect(),
    );
    let bytes = count(&[&prepared.reading], deadline)?;
    if bytes > prepared.bytes.saturating_add(RECOVERY_RESERVE) {
        return Err(incomplete(
            "OPC-UA recovery wrapper exceeds its reserved budget",
        ));
    }
    prepared.bytes = bytes;
    Ok(prepared)
}
pub(super) fn count(value: &impl serde::Serialize, deadline: Instant) -> Result<usize> {
    struct Limit {
        bytes: usize,
        deadline: Instant,
        expired: bool,
    }
    impl Write for Limit {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if Instant::now() >= self.deadline {
                self.expired = true;
                return Err(std::io::Error::other("OPC-UA deadline"));
            }
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|size| *size <= MAX_JSON)
                .ok_or_else(|| std::io::Error::other("OPC-UA JSON budget"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut limit = Limit {
        bytes: 0,
        deadline,
        expired: false,
    };
    if serde_json::to_writer(&mut limit, value).is_err() {
        return Err(if limit.expired {
            super::super::timeout_error()
        } else {
            incomplete("OPC-UA subscription envelope exceeds its JSON budget")
        });
    }
    Ok(limit.bytes)
}
