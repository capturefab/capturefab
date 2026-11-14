//! Native OS USB ownership through pinned nusb0.2.7. No retry or driver detach.
use super::{Config, wire};
use crate::types::{Categorized, ErrorKind};
use anyhow::Result;
use nusb::{
    Device, Endpoint, Interface, MaybeFuture as _,
    transfer::{Buffer, Bulk, ControlIn, ControlType, In, Out, Recipient},
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    In,
    Out,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct ControlRequest {
    pub(crate) request_type: u8,
    pub(crate) request: u8,
    pub(crate) value: u16,
    pub(crate) index: u16,
    pub(crate) length: u16,
}
pub(crate) struct Opened {
    pub(crate) vendor_id: u16,
    pub(crate) product_id: u16,
    pub(crate) serial_number: String,
    pub(crate) interface_number: u8,
    pub(crate) configuration: u8,
    pub(crate) alternate_setting: u8,
    pub(crate) class: u8,
    pub(crate) subclass: u8,
    pub(crate) protocol: u8,
    pub(crate) bulk_in: wire::Endpoint,
    pub(crate) bulk_out: wire::Endpoint,
    pub(crate) interrupt_in: Option<wire::Endpoint>,
}
pub(crate) struct Completion {
    pub(crate) data: Vec<u8>,
    pub(crate) transferred: usize,
    pub(crate) short_packet: bool,
    pub(crate) observed_ns: u64,
    pub(crate) status: Result<()>,
}
pub(crate) trait Backend {
    fn open(&mut self, selector: &Config, deadline: Instant) -> Result<Opened>;
    fn control_in(&mut self, request: ControlRequest, deadline: Instant) -> Result<Vec<u8>>;
    fn submit_out(&mut self, bytes: Vec<u8>, deadline: Instant) -> Result<()>;
    fn submit_in(&mut self, capacity: usize, deadline: Instant) -> Result<()>;
    fn wait(&mut self, direction: Direction, deadline: Instant) -> Result<Option<Completion>>;
    fn pending(&self, direction: Direction) -> usize;
    fn cancel(&mut self, direction: Direction, deadline: Instant) -> Result<()>;
    fn clear_halt(&mut self, direction: Direction, deadline: Instant) -> Result<()>;
    fn release(&mut self, deadline: Instant) -> Result<()>;
}
pub(crate) fn remaining(deadline: Instant) -> Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or_else(|| {
            Categorized::error(
                ErrorKind::Timeout,
                "USB operation original deadline expired",
            )
        })
}
fn native_error(operation: &'static str, error: &nusb::Error) -> anyhow::Error {
    let kind = match error.kind() {
        nusb::ErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        nusb::ErrorKind::NotFound | nusb::ErrorKind::Busy => ErrorKind::Unavailable,
        nusb::ErrorKind::Disconnected => ErrorKind::Disconnected,
        nusb::ErrorKind::Unsupported => ErrorKind::Unsupported,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::new(kind, format!("USB {operation}: {error}"))
        .details(serde_json::json!({"operation":operation,"nusb_error_kind":format!("{:?}",error.kind())})).into()
}
fn transfer_error(operation: &'static str, error: nusb::transfer::TransferError) -> anyhow::Error {
    use nusb::transfer::TransferError as Usb;
    let kind = match error {
        Usb::Disconnected => ErrorKind::Disconnected,
        Usb::Cancelled => ErrorKind::Timeout,
        _ => ErrorKind::OperationFailed,
    };
    Categorized::new(kind, format!("USB {operation}: {error}"))
        .details(
            serde_json::json!({"operation":operation,"nusb_transfer_status":format!("{error:?}")}),
        )
        .into()
}
#[derive(Default)]
pub(crate) struct Native {
    // Every successfully opened handle is retained, including a scan candidate
    // whose identity check subsequently fails. No blocking cleanup in parent.
    devices: Vec<Device>,
    selected: Option<usize>,
    selected_configuration: Option<u8>,
    interface: Option<Interface>,
    #[cfg(windows)]
    candidate_interfaces: Vec<Interface>,
    input: Option<Endpoint<Bulk, In>>,
    output: Option<Endpoint<Bulk, Out>>,
    input_capacity: Option<usize>,
    output_length: Option<usize>,
}
#[cfg(not(windows))]
fn standard_device(device: &Device, request: ControlIn, deadline: Instant) -> Result<Vec<u8>> {
    let bytes = device
        .control_in(request, remaining(deadline)?)
        .wait()
        .map_err(|error| transfer_error("standard descriptor read", error))?;
    remaining(deadline)?;
    Ok(bytes)
}
fn standard_interface(
    interface: &Interface,
    request: ControlIn,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let bytes = interface
        .control_in(request, remaining(deadline)?)
        .wait()
        .map_err(|error| transfer_error("claimed standard descriptor read", error))?;
    remaining(deadline)?;
    Ok(bytes)
}
fn standard_request(
    recipient: Recipient,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
) -> ControlIn {
    ControlIn {
        control_type: ControlType::Standard,
        recipient,
        request,
        value,
        index,
        length,
    }
}
fn device_header(bytes: &[u8]) -> Result<&[u8; 18]> {
    let bytes: &[u8; 18] = bytes
        .try_into()
        .map_err(|_| wire::invalid("incomplete USB device descriptor"))?;
    if bytes[0] != 18 || bytes[1] != 1 || bytes[16] == 0 {
        return Err(wire::invalid("invalid USB device serial descriptor"));
    }
    Ok(bytes)
}
fn decode_identity(raw: &[u8], serial: &[u8], config: &[u8]) -> Result<(u16, u16, String, u8)> {
    let bytes = device_header(raw)?;
    let head: &[u8; 2] = serial
        .get(..2)
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| wire::invalid("missing USB serial descriptor header"))?;
    let size = usize::from(head[0]);
    if head[1] != 3 || !(4..=128).contains(&size) || size != serial.len() {
        return Err(wire::invalid("invalid USB serial descriptor extent"));
    }
    let encoded = serial
        .get(2..size)
        .ok_or_else(|| wire::invalid("truncated USB serial"))?;
    let (words, tail) = encoded.as_chunks::<2>();
    if !tail.is_empty() {
        return Err(wire::invalid("odd USB serial descriptor"));
    }
    let mut text = String::with_capacity(words.len());
    for word in words {
        let code = u16::from_le_bytes(*word);
        if !(0x20..=0x7e).contains(&code) {
            return Err(wire::invalid(
                "US-English USB serial is not printable ASCII",
            ));
        }
        text.push(char::from(u8::try_from(code)?));
    }
    let [configuration] = config else {
        return Err(wire::invalid("incomplete USB configuration read"));
    };
    if *configuration == 0 {
        return Err(wire::invalid("USB device has no active configuration"));
    }
    Ok((
        u16::from_le_bytes([bytes[8], bytes[9]]),
        u16::from_le_bytes([bytes[10], bytes[11]]),
        text,
        *configuration,
    ))
}
#[cfg(not(windows))]
fn identity(device: &Device, deadline: Instant) -> Result<(u16, u16, String, u8)> {
    let raw = standard_device(
        device,
        standard_request(Recipient::Device, 6, 0x0100, 0, 18),
        deadline,
    )?;
    let index = device_header(&raw)?[16];
    let serial = standard_device(
        device,
        standard_request(Recipient::Device, 6, 0x0300 | u16::from(index), 0x0409, 128),
        deadline,
    )?;
    let config = standard_device(
        device,
        standard_request(Recipient::Device, 8, 0, 0, 1),
        deadline,
    )?;
    decode_identity(&raw, &serial, &config)
}
fn claimed_identity(
    interface: &Interface,
    number: u8,
    deadline: Instant,
) -> Result<(u16, u16, String, u8)> {
    let raw = standard_interface(
        interface,
        standard_request(Recipient::Device, 6, 0x0100, 0, 18),
        deadline,
    )?;
    let index = device_header(&raw)?[16];
    let serial = standard_interface(
        interface,
        standard_request(Recipient::Device, 6, 0x0300 | u16::from(index), 0x0409, 128),
        deadline,
    )?;
    let config = standard_interface(
        interface,
        standard_request(Recipient::Device, 8, 0, 0, 1),
        deadline,
    )?;
    let alternate = standard_interface(
        interface,
        standard_request(Recipient::Interface, 10, 0, u16::from(number), 1),
        deadline,
    )?;
    if alternate.as_slice() != [0] {
        return Err(wire::invalid("USB current alternate is not zero"));
    }
    decode_identity(&raw, &serial, &config)
}
fn matches_identity(actual: &(u16, u16, String, u8), selector: &Config) -> bool {
    (actual.0, actual.1, actual.2.as_str())
        == (
            selector.vendor_id,
            selector.product_id,
            selector.serial_number.as_str(),
        )
}
impl Native {
    fn device(&self) -> Result<&Device> {
        self.selected
            .and_then(|index| self.devices.get(index))
            .ok_or_else(|| wire::invalid("USB device is not selected"))
    }
    fn interface(&self) -> Result<&Interface> {
        self.interface
            .as_ref()
            .ok_or_else(|| wire::invalid("USB interface is not claimed"))
    }
    fn observed_descriptor(
        &self,
        selector: &Config,
        actual: (u16, u16, String, u8),
        deadline: Instant,
    ) -> Result<Opened> {
        let device = self.device()?;
        let (vendor, product, serial, configuration) = actual;
        if (vendor, product, serial.as_str())
            != (
                selector.vendor_id,
                selector.product_id,
                selector.serial_number.as_str(),
            )
        {
            return Err(wire::invalid("USB identity changed during interface claim"));
        }
        let active = device
            .configurations()
            .find(|value| value.configuration_value() == configuration)
            .ok_or_else(|| wire::invalid("USB active configuration descriptor is missing"))?;
        let mut alternatives = active.interface_alt_settings().filter(|value| {
            value.interface_number() == selector.interface_number && value.alternate_setting() == 0
        });
        let alternate = alternatives
            .next()
            .ok_or_else(|| wire::invalid("USB selected interface/alternate is absent"))?;
        if alternatives.next().is_some()
            || alternate.class() != 0xfe
            || alternate.subclass() != 3
            || !matches!(alternate.protocol(), 0 | 1)
        {
            return Err(wire::invalid(
                "USB selected interface is not a unique USBTMC interface",
            ));
        }
        let mut input = None;
        let mut output = None;
        let mut interrupt = None;
        for endpoint in alternate.endpoints() {
            let value = wire::Endpoint {
                address: endpoint.address(),
                max_packet_size: u16::try_from(endpoint.max_packet_size())?,
            };
            let slot = match (endpoint.transfer_type(), value.address & 0x80 != 0) {
                (nusb::descriptors::TransferType::Bulk, true) => &mut input,
                (nusb::descriptors::TransferType::Bulk, false) => &mut output,
                (nusb::descriptors::TransferType::Interrupt, true) => &mut interrupt,
                _ => return Err(wire::invalid("unexpected endpoint on USBTMC interface")),
            };
            if slot.replace(value).is_some() {
                return Err(wire::invalid("duplicate USBTMC endpoint"));
            }
        }
        let bulk_in = input.ok_or_else(|| wire::invalid("missing USB bulk input endpoint"))?;
        let bulk_out = output.ok_or_else(|| wire::invalid("missing USB bulk output endpoint"))?;
        if bulk_out.max_packet_size < 4
            || bulk_out.max_packet_size > 1024
            || bulk_out.max_packet_size % 4 != 0
            || bulk_in.max_packet_size == 0
            || bulk_in.max_packet_size > 1024
        {
            return Err(wire::invalid("invalid USBTMC bulk packet size"));
        }
        for endpoint in [Some(bulk_in), Some(bulk_out), interrupt]
            .into_iter()
            .flatten()
        {
            if endpoint.address & 0x70 != 0
                || endpoint.address & 15 == 0
                || endpoint.max_packet_size == 0
                || endpoint.max_packet_size > 1024
            {
                return Err(wire::invalid("invalid USB endpoint address or packet size"));
            }
        }
        if interrupt.is_some_and(|value| value.address == bulk_in.address) {
            return Err(wire::invalid("duplicate USB input address"));
        }
        remaining(deadline)?;
        Ok(Opened {
            vendor_id: vendor,
            product_id: product,
            serial_number: serial,
            interface_number: selector.interface_number,
            configuration,
            alternate_setting: 0,
            class: alternate.class(),
            subclass: alternate.subclass(),
            protocol: alternate.protocol(),
            bulk_in,
            bulk_out,
            interrupt_in: interrupt,
        })
    }
}
impl Backend for Native {
    fn open(&mut self, selector: &Config, deadline: Instant) -> Result<Opened> {
        selector.validate()?;
        remaining(deadline)?;
        if !self.devices.is_empty() || self.interface.is_some() {
            return Err(wire::invalid("USB backend was already opened"));
        }
        let devices = nusb::list_devices()
            .wait()
            .map_err(|error| native_error("enumerate", &error))?;
        remaining(deadline)?;
        for info in devices {
            remaining(deadline)?;
            if info.vendor_id() != selector.vendor_id || info.product_id() != selector.product_id {
                continue;
            }
            #[cfg(windows)]
            if info
                .serial_number()
                .is_some_and(|serial| serial != selector.serial_number.as_str())
            {
                // Pinned Windows enumeration reads this exact0409 USB string
                // through the hub, not from a normalized PnP instance ID.
                continue;
            }
            if self.devices.len() >= 4096 {
                return Err(wire::invalid(
                    "USB identity enumeration exceeds its bounded candidate limit",
                ));
            }
            let device = info
                .open()
                .wait()
                .map_err(|error| native_error("open", &error))?;
            self.devices.push(device);
            let index = self.devices.len() - 1;
            remaining(deadline)?;
            let device = self
                .devices
                .get(index)
                .ok_or_else(|| wire::invalid("USB candidate handle disappeared"))?;
            #[cfg(not(windows))]
            {
                let actual = identity(device, deadline)?;
                if matches_identity(&actual, selector) {
                    self.selected_configuration = Some(actual.3);
                    if self.selected.replace(index).is_some() {
                        return Err(wire::invalid("ambiguous exact USBTMC identity"));
                    }
                    self.observed_descriptor(selector, actual, deadline)?;
                    let alternate = standard_device(
                        self.device()?,
                        standard_request(
                            Recipient::Interface,
                            10,
                            0,
                            u16::from(selector.interface_number),
                            1,
                        ),
                        deadline,
                    )?;
                    if alternate.as_slice() != [0] {
                        return Err(wire::invalid(
                            "USB current alternate is not zero before claim",
                        ));
                    }
                }
            }
            #[cfg(windows)]
            {
                // nusb0.2.7 cannot send Device controls on Windows. Do not use a
                // negative cached serial as actual0409 identity: plain-claim
                // VID/PID candidates, then qualify before any class/query I/O.
                let active = device.active_configuration().map_err(|error| {
                    Categorized::error(
                        ErrorKind::Unavailable,
                        format!("USB active configuration unavailable: {error}"),
                    )
                })?;
                if !active.interface_alt_settings().any(|value| {
                    value.interface_number() == selector.interface_number
                        && value.alternate_setting() == 0
                        && value.class() == 0xfe
                        && value.subclass() == 3
                        && matches!(value.protocol(), 0 | 1)
                }) {
                    return Err(wire::invalid(
                        "Windows USB candidate lacks the requested USBTMC interface",
                    ));
                }
                let before_configuration = active.configuration_value();
                let interface = device
                    .claim_interface(selector.interface_number)
                    .wait()
                    .map_err(|error| native_error("plain interface claim", &error))?;
                self.candidate_interfaces.push(interface);
                remaining(deadline)?;
                let interface = self
                    .candidate_interfaces
                    .last()
                    .ok_or_else(|| wire::invalid("USB claimed candidate disappeared"))?;
                let actual = claimed_identity(interface, selector.interface_number, deadline)?;
                if actual.3 != before_configuration {
                    return Err(wire::invalid(
                        "Windows USB active configuration changed during claim",
                    ));
                }
                if matches_identity(&actual, selector) {
                    self.selected_configuration = Some(actual.3);
                    if self.selected.replace(index).is_some() {
                        return Err(wire::invalid("ambiguous exact USBTMC identity"));
                    }
                    self.interface = Some(interface.clone());
                    self.observed_descriptor(selector, actual, deadline)?;
                }
                // The selected clone remains owned; unselected claims are
                // released here. A later failure retains all other partials.
                drop(self.candidate_interfaces.pop());
                remaining(deadline)?;
            }
        }
        if self.selected.is_none() {
            return Err(Categorized::error(
                ErrorKind::Unavailable,
                "selected USBTMC identity was not found",
            ));
        }
        #[cfg(not(windows))]
        {
            self.interface = Some(
                self.device()?
                    .claim_interface(selector.interface_number)
                    .wait()
                    .map_err(|error| native_error("plain interface claim", &error))?,
            );
            remaining(deadline)?;
        }
        let actual = claimed_identity(self.interface()?, selector.interface_number, deadline)?;
        if self.selected_configuration != Some(actual.3) {
            return Err(wire::invalid(
                "USB active configuration changed during claim",
            ));
        }
        let descriptor = self.observed_descriptor(selector, actual, deadline)?;
        self.input = Some(
            self.interface()?
                .endpoint::<Bulk, In>(descriptor.bulk_in.address)
                .map_err(|error| native_error("open input endpoint", &error))?,
        );
        self.output = Some(
            self.interface()?
                .endpoint::<Bulk, Out>(descriptor.bulk_out.address)
                .map_err(|error| native_error("open output endpoint", &error))?,
        );
        remaining(deadline)?;
        Ok(descriptor)
    }
    fn control_in(&mut self, request: ControlRequest, deadline: Instant) -> Result<Vec<u8>> {
        let recipient = match request.request_type & 0x1f {
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            _ => return Err(wire::invalid("invalid USBTMC control recipient")),
        };
        if request.request_type & 0xe0 != 0xa0 {
            return Err(wire::invalid("invalid USBTMC control request type"));
        }
        let bytes = self
            .interface()?
            .control_in(
                ControlIn {
                    control_type: ControlType::Class,
                    recipient,
                    request: request.request,
                    value: request.value,
                    index: request.index,
                    length: request.length,
                },
                remaining(deadline)?,
            )
            .wait()
            .map_err(|error| transfer_error("class control", error))?;
        remaining(deadline)?;
        Ok(bytes)
    }
    fn submit_out(&mut self, bytes: Vec<u8>, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        if self.pending(Direction::Out) != 0 {
            return Err(wire::invalid("USB output transfer already pending"));
        }
        self.output_length = Some(bytes.len());
        self.output
            .as_mut()
            .ok_or_else(|| wire::invalid("USB output endpoint is closed"))?
            .submit(bytes.into());
        remaining(deadline)?;
        Ok(())
    }
    fn submit_in(&mut self, capacity: usize, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        if self.pending(Direction::In) != 0 || !(1..=68_608).contains(&capacity) {
            return Err(wire::invalid("invalid USB pending input capacity"));
        }
        self.input_capacity = Some(capacity);
        self.input
            .as_mut()
            .ok_or_else(|| wire::invalid("USB input endpoint is closed"))?
            .submit(Buffer::new(capacity));
        remaining(deadline)?;
        Ok(())
    }
    fn wait(&mut self, direction: Direction, deadline: Instant) -> Result<Option<Completion>> {
        let left = remaining(deadline)?;
        if self.pending(direction) == 0 {
            return Err(wire::invalid("USB wait has no pending transfer"));
        }
        let completion = match direction {
            Direction::In => self
                .input
                .as_mut()
                .ok_or_else(|| wire::invalid("USB input is closed"))?
                .wait_next_complete(left),
            Direction::Out => self
                .output
                .as_mut()
                .ok_or_else(|| wire::invalid("USB output is closed"))?
                .wait_next_complete(left),
        };
        let Some(completion) = completion else {
            return Ok(None);
        };
        let observed_ns = wire::now()?;
        let limit = match direction {
            Direction::In => self.input_capacity.take(),
            Direction::Out => self.output_length.take(),
        }
        .ok_or_else(|| wire::invalid("USB completion has no submitted buffer"))?;
        if completion.actual_len > limit
            || (direction == Direction::In && completion.actual_len > completion.buffer.len())
        {
            return Err(wire::invalid(
                "native USB completion exceeds its initialized buffer",
            ));
        }
        let transferred = completion.actual_len;
        let short_packet =
            direction == Direction::In && completion.status.is_ok() && transferred < limit;
        let data = if direction == Direction::In {
            completion
                .buffer
                .get(..transferred)
                .ok_or_else(|| wire::invalid("USB initialized prefix is unavailable"))?
                .to_vec()
        } else {
            Vec::new()
        };
        let status = completion
            .status
            .map_err(|error| transfer_error("bulk completion", error));
        Ok(Some(Completion {
            data,
            transferred,
            short_packet,
            observed_ns,
            status,
        }))
    }
    fn pending(&self, direction: Direction) -> usize {
        match direction {
            Direction::In => self.input.as_ref().map_or(0, Endpoint::pending),
            Direction::Out => self.output.as_ref().map_or(0, Endpoint::pending),
        }
    }
    fn cancel(&mut self, direction: Direction, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        match direction {
            Direction::In => {
                if let Some(endpoint) = &mut self.input {
                    endpoint.cancel_all();
                }
            }
            Direction::Out => {
                if let Some(endpoint) = &mut self.output {
                    endpoint.cancel_all();
                }
            }
        }
        remaining(deadline)?;
        Ok(())
    }
    fn clear_halt(&mut self, direction: Direction, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        if self.pending(direction) != 0 {
            return Err(wire::invalid("USB halt clear while transfer is pending"));
        }
        match direction {
            Direction::In => self
                .input
                .as_mut()
                .ok_or_else(|| wire::invalid("USB input is closed"))?
                .clear_halt()
                .wait(),
            Direction::Out => self
                .output
                .as_mut()
                .ok_or_else(|| wire::invalid("USB output is closed"))?
                .clear_halt()
                .wait(),
        }
        .map_err(|error| native_error("clear endpoint halt", &error))?;
        remaining(deadline)?;
        Ok(())
    }
    fn release(&mut self, deadline: Instant) -> Result<()> {
        remaining(deadline)?;
        drop(self.input.take());
        drop(self.output.take());
        drop(self.interface.take());
        #[cfg(windows)]
        self.candidate_interfaces.clear();
        self.selected = None;
        self.selected_configuration = None;
        self.devices.clear();
        self.input_capacity = None;
        self.output_length = None;
        remaining(deadline)?;
        Ok(())
    }
}
