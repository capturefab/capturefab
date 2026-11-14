//! One outstanding request on one private monitor connection; no cleanup command.
use super::{MAX_RAW, MAX_REPLY, Terminator, clock, incomplete, net, remaining};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use serde::Serialize;
use std::{
    io,
    net::{SocketAddr, TcpStream},
    time::Instant,
};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Serialize)]
pub(super) struct Window {
    pub(super) start: u64,
    pub(super) end: u64,
}
pub(super) struct Reply {
    pub(super) raw: Zeroizing<Vec<u8>>,
    pub(super) window: Window,
}
pub(super) struct Link {
    wire: Box<dyn net::Transport>,
}
impl Link {
    #[cfg(test)]
    pub(super) fn from_transport(wire: Box<dyn net::Transport>) -> Self {
        Self { wire }
    }
    pub(super) fn connect(address: SocketAddr, deadline: Instant) -> Result<Self> {
        let socket = TcpStream::connect_timeout(&address, remaining(deadline)?)
            .map_err(|error| io_error(&error))?;
        socket.set_nodelay(true).map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        Ok(Self {
            wire: net::plain(socket, deadline),
        })
    }
    pub(super) fn exchange(
        &mut self,
        request: &[u8],
        terminator: Terminator,
        deadline: Instant,
        raw_bytes: &mut usize,
    ) -> Result<Reply> {
        remaining(deadline)?;
        self.wire.set_deadline(deadline);
        let start = clock()?;
        self.wire
            .write_all(request)
            .map_err(|error| io_error(&error))?;
        remaining(deadline)?;
        let mut raw = Zeroizing::new(Vec::with_capacity(512));
        let mut incoming = Zeroizing::new([0_u8; 512]);
        loop {
            remaining(deadline)?;
            let count = match self.wire.read(incoming.as_mut()) {
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(&error)),
            };
            remaining(deadline)?;
            if count == 0 {
                return Err(incomplete("connection ended before its complete reply"));
            }
            *raw_bytes = raw_bytes.saturating_add(count);
            if *raw_bytes > MAX_RAW {
                return Err(incomplete(
                    "poll responses exceed the 512 KiB raw byte bound",
                ));
            }
            if raw.len().saturating_add(count) > MAX_REPLY {
                return Err(incomplete("reply exceeds 16 KiB including its terminator"));
            }
            let search_start = raw.len().saturating_sub(terminator.bytes().len() - 1);
            raw.extend_from_slice(
                incoming
                    .get(..count)
                    .ok_or_else(|| incomplete("invalid receive extent"))?,
            );
            if let Some(relative) = raw
                .get(search_start..)
                .ok_or_else(|| incomplete("invalid framing search extent"))?
                .windows(terminator.bytes().len())
                .position(|bytes| bytes == terminator.bytes())
            {
                let at = search_start + relative;
                if at + terminator.bytes().len() != raw.len() {
                    return Err(incomplete("unsolicited bytes follow the outstanding reply"));
                }
                let end = clock()?;
                remaining(deadline)?;
                return Ok(Reply {
                    raw,
                    window: Window { start, end },
                });
            }
        }
    }
}
fn io_error(error: &io::Error) -> anyhow::Error {
    let kind = match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ErrorKind::Timeout,
        io::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Unavailable,
    };
    Categorized::error(kind, "Epson monitor transport failed")
}
