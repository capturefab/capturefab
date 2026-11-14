//! Direct Micro800 connected explicit transport. See the pinned libplctag
//! session.c and cip.c, and ODVA PUB00123R1 §3.3.11.2.3.
use super::{Connection, cip_reply, cpf, protocol};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

pub(super) const SEND_UNIT: u16 = 0x70;
pub(super) const MAX_PAYLOAD: usize = 504;
pub(super) const CIP_OFFSET: usize = 22;
const MANAGER: [u8; 5] = [2, 0x20, 6, 0x24, 1];
const ROUTER: [u8; 4] = [0x20, 2, 0x24, 1];
const OPEN: u8 = 0x54;
const CLOSE: u8 = 0x4e;
const TICKS: [u8; 2] = [0x0a, 0x0e];
const MULTIPLIER: u8 = 3;
const REQUESTED_INTERVAL_US: u32 = 1_000_000;
const CLOSE_BUDGET: Duration = Duration::from_millis(200);

pub(super) struct Class3 {
    identity: [u8; 8],
    to_target: u32,
    to_originator: u32,
    sequence: u16,
    refresh_after: Duration,
    last_request: Instant,
}

pub(super) fn ensure(
    connection: &mut Option<Connection>,
    address: SocketAddr,
    vendor_id: u16,
    context: &mut u64,
    deadline: Instant,
) -> Result<()> {
    if connection.as_ref().is_some_and(Connection::class3_fresh) {
        return Ok(());
    }
    // An inactive Class 3 connection may already be gone at the target.
    // Closing the old TCP session prevents it from carrying new traffic.
    drop(connection.take());
    let mut opened = Connection::connect(address, deadline)?;
    opened.open_class3(vendor_id, context, deadline)?;
    *connection = Some(opened);
    Ok(())
}

impl Connection {
    fn class3_fresh(&self) -> bool {
        self.class3
            .as_ref()
            .is_some_and(|state| state.last_request.elapsed() < state.refresh_after)
    }

    fn open_class3(&mut self, vendor_id: u16, context: &mut u64, deadline: Instant) -> Result<()> {
        let mut random = [0; 10];
        getrandom::fill(&mut random).map_err(|error| {
            Categorized::error(
                ErrorKind::Unavailable,
                format!("could not create Micro800 connection identity: {error}"),
            )
        })?;
        let serial = u16::from_le_bytes(random[..2].try_into()?).max(1);
        let originator = u32::from_le_bytes(random[2..6].try_into()?).max(1);
        let receive_id = u32::from_le_bytes(random[6..].try_into()?).max(1);
        let mut identity = [0; 8];
        identity[..2].copy_from_slice(&serial.to_le_bytes());
        identity[2..4].copy_from_slice(&vendor_id.to_le_bytes());
        identity[4..].copy_from_slice(&originator.to_le_bytes());

        let mut request = manager_request(OPEN);
        request.extend_from_slice(&[0; 4]); // The target supplies its receive ID.
        request.extend_from_slice(&receive_id.to_le_bytes());
        request.extend_from_slice(&identity);
        request.extend_from_slice(&[MULTIPLIER, 0, 0, 0]);
        let parameters = 0x4200 | u16::try_from(MAX_PAYLOAD)?;
        for _ in 0..2 {
            request.extend_from_slice(&REQUESTED_INTERVAL_US.to_le_bytes());
            request.extend_from_slice(&parameters.to_le_bytes());
        }
        request.extend_from_slice(&[0xa3, 2]); // Server, application trigger, Class 3.
        request.extend_from_slice(&ROUTER);

        let began = Instant::now();
        let body = self.exchange_ucmm(&request, None, context, deadline)?;
        let reply = cip_reply(cpf(&body)?, OPEN | 0x80, false)?;
        let Some((header, application)) = reply.data.split_first_chunk::<26>() else {
            return Err(protocol("invalid Micro800 Forward Open reply length"));
        };
        if reply.partial || header[8..16] != identity {
            return Err(protocol(
                "invalid Micro800 Forward Open reply identity or length",
            ));
        }
        if header[25] != 0 {
            return Err(protocol("invalid Micro800 Forward Open application reply"));
        }
        if application.len() != usize::from(header[24]) * 2 {
            return Err(protocol("invalid Micro800 Forward Open application length"));
        }
        let to_target = u32::from_le_bytes(header[..4].try_into()?);
        let to_originator = u32::from_le_bytes(header[4..8].try_into()?);
        if to_target == 0 || to_originator == 0 {
            return Err(protocol(
                "Micro800 Forward Open returned a zero connection ID",
            ));
        }
        let outbound_us = u32::from_le_bytes(header[16..20].try_into()?);
        let inbound_us = u32::from_le_bytes(header[20..24].try_into()?);
        let interval = outbound_us.min(inbound_us).min(REQUESTED_INTERVAL_US);
        // Renew at half the shorter watchdog, or before the next poll if
        // the target reports no interval. Count from send, not arrival.
        let refresh_after = Duration::from_micros(u64::from(interval) * (2 << MULTIPLIER));
        self.class3 = Some(Class3 {
            identity,
            to_target,
            to_originator,
            sequence: 0,
            refresh_after,
            last_request: began,
        });
        Ok(())
    }

    pub(super) fn exchange_connected(
        &mut self,
        request: &[u8],
        deadline: Instant,
    ) -> Result<Vec<u8>> {
        let state = self
            .class3
            .as_mut()
            .ok_or_else(|| protocol("missing Micro800 Class 3 connection"))?;
        let size = request.len() + 2;
        if size > MAX_PAYLOAD {
            return Err(Usage("Micro800 connected request exceeds 504 bytes".into()).into());
        }
        state.sequence = state.sequence.wrapping_add(1);
        let sequence = state.sequence.to_le_bytes();
        let reply_id = state.to_originator.to_le_bytes();
        state.last_request = Instant::now();

        let mut body = vec![0, 0, 0, 0, 0, 0, 2, 0, 0xa1, 0, 4, 0];
        body.extend_from_slice(&state.to_target.to_le_bytes());
        body.extend_from_slice(&[0xb1, 0]);
        body.extend_from_slice(&u16::try_from(size)?.to_le_bytes());
        body.extend_from_slice(&sequence);
        body.extend_from_slice(request);
        // SendUnitData does not use encapsulation Sender Context. Its
        // connection ID and sequence identify the synchronous response.
        let response = self.exchange(SEND_UNIT, &body, 0, deadline)?;
        let [
            0,
            0,
            0,
            0,
            0,
            0,
            2,
            0,
            0xa1,
            0,
            4,
            0,
            id0,
            id1,
            id2,
            id3,
            0xb1,
            0,
            lo,
            hi,
            seq0,
            seq1,
            data @ ..,
        ] = response.as_slice()
        else {
            return Err(protocol("invalid Micro800 connected Common Packet Format"));
        };
        if [*id0, *id1, *id2, *id3] != reply_id || [*seq0, *seq1] != sequence {
            return Err(protocol("unexpected Micro800 connection ID or sequence"));
        }
        let declared = usize::from(u16::from_le_bytes([*lo, *hi]));
        if declared != data.len() + 2 || declared > MAX_PAYLOAD {
            return Err(protocol("invalid Micro800 connected data length"));
        }
        Ok(response)
    }

    pub(super) fn finish_class3(&mut self, context: &mut u64, timeout: Duration) {
        let Some(state) = self.class3.take() else {
            return;
        };
        let timeout = timeout.min(CLOSE_BUDGET);
        if timeout.is_zero() {
            return;
        }
        let deadline = Instant::now() + timeout;
        // Explicit close is best effort. TCP close and the controller's
        // watchdog still reclaim resources if the reply is lost or late.
        drop(self.close_class3(state.identity, context, deadline));
    }

    fn close_class3(
        &mut self,
        identity: [u8; 8],
        context: &mut u64,
        deadline: Instant,
    ) -> Result<()> {
        let mut request = manager_request(CLOSE);
        request.extend_from_slice(&identity);
        request.extend_from_slice(&[2, 0]);
        request.extend_from_slice(&ROUTER);
        let body = self.exchange_ucmm(&request, None, context, deadline)?;
        let reply = cip_reply(cpf(&body)?, CLOSE | 0x80, false)?;
        if reply.partial || reply.data.len() < 10 || reply.data[..8] != identity {
            return Err(protocol(
                "invalid Micro800 Forward Close reply identity or length",
            ));
        }
        let [words, 0, application @ ..] = &reply.data[8..] else {
            return Err(protocol("invalid Micro800 Forward Close application reply"));
        };
        if application.len() != usize::from(*words) * 2 {
            return Err(protocol(
                "invalid Micro800 Forward Close application length",
            ));
        }
        Ok(())
    }
}

fn manager_request(service: u8) -> Vec<u8> {
    let mut request = vec![service];
    request.extend_from_slice(&MANAGER);
    request.extend_from_slice(&TICKS);
    request
}
