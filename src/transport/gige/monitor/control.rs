//! Read-only GVCP construction. There is no owner lease, write opcode or release path.
use super::super::{
    GVCP_PORT, GvcpStatus, GvcpTimeout, MEMORY_CHUNK, PENDING_ACK, READ_MEMORY, READ_REGISTER,
    command_packet, memory_range, parse_ack,
};
use super::{MonitorSpec, before, denied, receive};
use crate::types::RegisterIo;
use anyhow::{Context as _, Result, ensure};
use std::{
    net::{SocketAddrV4, UdpSocket},
    time::{Duration, Instant},
};

pub(super) struct ReadControl {
    socket: UdpSocket,
    evidence: receive::Evidence,
    interface: super::super::bootstrap::Interface,
    peer: SocketAddrV4,
    next_id: u16,
    pub(super) initial: Option<Instant>,
    pub(super) timeout: Duration,
    pub(super) legacy: bool,
}
impl ReadControl {
    pub(super) fn new(
        spec: &MonitorSpec,
        interface: &super::super::bootstrap::Interface,
        until: Instant,
    ) -> Result<Self> {
        let timeout = before(until)?.min(Duration::from_secs(30));
        let socket = receive::control_socket(interface)?;
        let peer = SocketAddrV4::new(spec.camera, GVCP_PORT);
        socket.connect(peer)?;
        let evidence = receive::Evidence::new(&socket)?;
        before(until)?;
        Ok(Self {
            socket,
            evidence,
            interface: interface.clone(),
            peer,
            next_id: 1,
            initial: Some(until),
            timeout,
            legacy: true,
        })
    }
    #[cfg(test)]
    pub(super) fn test_socket(socket: UdpSocket, until: Instant) -> Result<Self> {
        let std::net::SocketAddr::V4(local) = socket.local_addr()? else {
            anyhow::bail!("IPv4 fixture socket required");
        };
        let std::net::SocketAddr::V4(peer) = socket.peer_addr()? else {
            anyhow::bail!("IPv4 fixture peer required");
        };
        let interface = MonitorSpec {
            camera: *peer.ip(),
            interface: *local.ip(),
            mac: [2, 0, 0, 0, 0, 1],
        }
        .resolve()?;
        let evidence = receive::Evidence::new(&socket)?;
        Ok(Self {
            socket,
            evidence,
            interface,
            peer,
            next_id: 1,
            initial: Some(until),
            timeout: Duration::from_secs(1),
            legacy: false,
        })
    }
    fn exchange(&mut self, command: u16, payload: &[u8], until: Instant) -> Result<Vec<u8>> {
        ensure!(matches!(command, READ_REGISTER | READ_MEMORY), denied());
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let packet = command_packet(command, id, 1, payload)?;
        let started = Instant::now();
        let interval = (before(until)? / 3).max(Duration::from_millis(1));
        let mut buffer = vec![0_u8; 65_536];
        let mut received = 0_usize;
        for _ in 0..3 {
            self.socket.set_write_timeout(Some(before(until)?))?;
            ensure!(
                self.socket.send(&packet)? == packet.len(),
                "short monitor GVCP read send"
            );
            let mut attempt = (Instant::now() + interval).min(until);
            loop {
                let remaining = attempt.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                self.socket.set_read_timeout(Some(remaining))?;
                let packet = match self.evidence.receive(&self.socket, &mut buffer) {
                    Ok(Some(packet)) => packet,
                    Ok(None) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if super::super::is_receive_timeout(&error) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                        return Err(super::incomplete("invalid monitor GVCP receive evidence")
                            .context(error));
                    }
                    Err(error) => return Err(error.into()),
                };
                before(until)?;
                received += 1;
                ensure!(
                    received <= 1024,
                    super::incomplete("monitor GVCP reply work bound exceeded")
                );
                if packet.source != self.peer
                    || packet.destination != self.interface.address
                    || packet.interface_index != self.interface.index
                {
                    continue;
                }
                let raw = buffer.get(..packet.bytes).ok_or_else(|| {
                    super::incomplete("monitor GVCP reply exceeds receive buffer")
                })?;
                let Some(header) = raw.first_chunk::<8>() else {
                    continue;
                };
                if u16::from_be_bytes([header[6], header[7]]) != id {
                    continue;
                }
                let ack = parse_ack(raw)
                    .map_err(|_| super::incomplete("malformed correlated monitor GVCP reply"))?;
                if ack.command != command + 1 && ack.command != PENDING_ACK {
                    continue;
                }
                if ack.status != 0 {
                    return Err(GvcpStatus(ack.status).into());
                }
                if ack.command == PENDING_ACK {
                    let data = ack
                        .payload
                        .first_chunk::<4>()
                        .filter(|_| ack.payload.len() == 4)
                        .context("invalid monitor pending acknowledgement")?;
                    let delay =
                        Duration::from_millis(u64::from(u16::from_be_bytes([data[2], data[3]])));
                    attempt = (Instant::now() + delay + interval).min(until);
                    continue;
                }
                let value = ack.payload.to_vec();
                before(until)?;
                return Ok(value);
            }
        }
        before(until)?;
        Err(GvcpTimeout {
            command,
            waited: started.elapsed(),
        }
        .into())
    }
    pub(super) fn register(&mut self, address: u32, until: Instant) -> Result<u32> {
        ensure!(address.is_multiple_of(4), "unaligned monitor register read");
        let raw = self.exchange(READ_REGISTER, &address.to_be_bytes(), until)?;
        let bytes: [u8; 4] = raw.try_into().map_err(|_| {
            super::incomplete("monitor register acknowledgement must contain one word")
        })?;
        Ok(u32::from_be_bytes(bytes))
    }
    pub(super) fn memory(
        &mut self,
        address: u64,
        length: usize,
        until: Instant,
    ) -> Result<Vec<u8>> {
        before(until)?;
        let (base, aligned, skip) = memory_range(address, length)?;
        let mut bytes = Vec::with_capacity(aligned);
        if aligned == 4 && self.legacy {
            bytes.extend_from_slice(&self.register(base, until)?.to_be_bytes());
        } else {
            for offset in (0..aligned).step_by(MEMORY_CHUNK) {
                let count = (aligned - offset).min(MEMORY_CHUNK);
                let current = base
                    .checked_add(u32::try_from(offset)?)
                    .context("monitor memory address overflow")?;
                let mut request = current.to_be_bytes().to_vec();
                request.extend_from_slice(&u32::try_from(count)?.to_be_bytes());
                let ack = self.exchange(READ_MEMORY, &request, until)?;
                ensure!(
                    ack.len() == count + 4,
                    super::incomplete("monitor memory acknowledgement extent mismatch")
                );
                let (returned, data) = ack.split_at(4);
                ensure!(
                    returned == current.to_be_bytes(),
                    super::incomplete("monitor memory acknowledgement address mismatch")
                );
                bytes.extend_from_slice(data);
            }
        }
        bytes.copy_within(skip..skip + length, 0);
        bytes.truncate(length);
        before(until)?;
        Ok(bytes)
    }
    pub(super) fn deadline(&self) -> Instant {
        self.initial
            .unwrap_or_else(|| Instant::now() + self.timeout)
    }
}
impl RegisterIo for ReadControl {
    fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
        self.memory(address, length, self.deadline())
    }
    fn read_memory_until(
        &mut self,
        address: u64,
        length: usize,
        until: Instant,
    ) -> Result<Vec<u8>> {
        self.memory(
            address,
            length,
            self.initial.map_or(until, |initial| initial.min(until)),
        )
    }
    fn write_memory(&mut self, _address: u64, _data: &[u8]) -> Result<()> {
        Err(denied())
    }
    fn write_memory_until(&mut self, _address: u64, _data: &[u8], _until: Instant) -> Result<()> {
        Err(denied())
    }
}
