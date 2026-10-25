//! Human tables for device lists. `--json` output never passes through here.
use crate::types::Transport;
use serde_json::Value;
use std::io::{self, Write};

/// Which command listed the devices; each says something different when
/// the list is empty and suggests a different next step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeviceList {
    Discover,
    Native,
    Scanners,
    Onvif,
}

/// IDs are never truncated, since people copy them; a longer one overflows
/// its row instead of widening every row.
const ID_WIDTH: usize = 48;
const MODEL_WIDTH: usize = 32;
const HOST_PREFIXES: [&str; 3] = ["avfoundation:", "v4l2:", "dshow:"];

fn text<'a>(device: &'a Value, key: &str) -> &'a str {
    device[key].as_str().unwrap_or("")
}

fn host_device(id: &str) -> bool {
    HOST_PREFIXES.iter().any(|prefix| id.starts_with(prefix))
}

/// A webcam or screen on this computer, as AVFoundation names screens.
fn screen(device: &Value) -> bool {
    host_device(text(device, "id"))
        && device["model"]
            .as_str()
            .or_else(|| device["name"].as_str())
            .is_some_and(|name| name.starts_with("Capture screen"))
}

/// Plain transport names: "GigE" rather than the JSON's `gig_e`, and
/// "Webcam" rather than "Media" for a camera on this computer.
fn transport_label(device: &Value) -> String {
    let raw = text(device, "transport");
    let id = text(device, "id");
    match serde_json::from_value::<Transport>(device["transport"].clone()) {
        Ok(Transport::Media) if screen(device) => "Screen".into(),
        Ok(Transport::Media) if host_device(id) => "Webcam".into(),
        Ok(Transport::Media) if crate::hubs::is_source(id) => "Hub".into(),
        Ok(transport) => transport.to_string(),
        Err(_) => raw.into(),
    }
}

fn ellipsize(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.into()
    } else {
        let mut short: String = value.chars().take(width.saturating_sub(1)).collect();
        short.push('…');
        short
    }
}

/// Columns sized to their cells, dropping any that are empty in every row.
fn write_table(out: &mut impl Write, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    let caps = [ID_WIDTH, usize::MAX, MODEL_WIDTH, usize::MAX, usize::MAX];
    let shown: Vec<usize> = (0..headers.len())
        .filter(|&column| column == 0 || rows.iter().any(|row| !row[column].is_empty()))
        .collect();
    let widths: Vec<usize> = (0..headers.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain([headers[column].len()])
                .max()
                .unwrap_or(0)
                .min(caps.get(column).copied().unwrap_or(usize::MAX))
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let mut line = String::new();
        for (position, &column) in shown.iter().enumerate() {
            let cell = cells[column];
            if position + 1 == shown.len() {
                line.push_str(cell);
            } else {
                line.push_str(cell);
                let used = cell.chars().count();
                line.push_str(&" ".repeat(widths[column].saturating_sub(used) + 2));
            }
        }
        line.trim_end().to_owned()
    };
    writeln!(out, "{}", line(headers.to_vec()))?;
    for row in rows {
        writeln!(out, "{}", line(row.iter().map(String::as_str).collect()))?;
    }
    Ok(())
}

fn camera_row(device: &Value) -> Vec<String> {
    let id = crate::media::redact_url(text(device, "id"));
    let address = device["address"]
        .as_str()
        .filter(|address| *address != text(device, "id"))
        .map(crate::media::redact_url)
        .unwrap_or_default();
    let serial = text(device, "serial");
    // A serial the ID already spells out (scanner:ica:UUID) adds nothing.
    let serial =
        if text(device, "id").ends_with(serial) || Some(serial) == device["address"].as_str() {
            String::new()
        } else {
            serial.into()
        };
    let model = device["model"]
        .as_str()
        .or_else(|| device["name"].as_str())
        .unwrap_or("");
    vec![
        id,
        transport_label(device),
        ellipsize(model, MODEL_WIDTH),
        address,
        serial,
    ]
}

fn onvif_name(device: &Value) -> String {
    device["scopes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .find_map(|scope| scope.strip_prefix("onvif://www.onvif.org/name/"))
        .map(|name| name.replace("%20", " ").replace('_', " "))
        .unwrap_or_default()
}

/// The device table on stdout. Next-step hints are separate (see
/// [`next_steps`]), so a pipe receives only the table.
pub(super) fn write_device_table(
    out: &mut impl Write,
    devices: &[Value],
    kind: DeviceList,
) -> io::Result<()> {
    if devices.is_empty() {
        let lines: &[&str] = match kind {
            DeviceList::Discover => &[
                "No cameras found.",
                "GigE: check the network with `capturefab network-scan` and `capturefab doctor`.",
                "IP cameras: --camera rtsp://HOST/PATH or `capturefab onvif discover`.",
                "Try the simulator: --camera sim:0 (or discover --simulate).",
            ],
            DeviceList::Native => &[
                "No host cameras found. Check that a webcam is attached and that this terminal may use the camera.",
            ],
            DeviceList::Scanners => &["No document scanners found."],
            DeviceList::Onvif => &[
                "No ONVIF devices answered.",
                "Discovery uses multicast on this subnet; for a camera on another subnet or VLAN, run capturefab onvif resolve http://HOST/onvif/device_service.",
            ],
        };
        for line in lines {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }
    if kind == DeviceList::Onvif {
        let rows: Vec<_> = devices
            .iter()
            .map(|device| {
                let addresses = device["addresses"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .filter(|address| *address != text(device, "endpoint"))
                    .map(crate::media::redact_url)
                    .collect::<Vec<_>>()
                    .join(" ");
                vec![
                    crate::media::redact_url(text(device, "endpoint")),
                    onvif_name(device),
                    addresses,
                ]
            })
            .collect();
        return write_table(out, &["ENDPOINT", "NAME", "OTHER ADDRESSES"], &rows);
    }
    let rows: Vec<_> = devices.iter().map(camera_row).collect();
    write_table(
        out,
        &["ID", "TRANSPORT", "MODEL", "ADDRESS", "SERIAL"],
        &rows,
    )
}

/// Hints for stderr after a device table: what to run next, and for
/// discover, where to look when no machine-vision camera answered.
pub(super) fn next_steps(devices: &[Value], kind: DeviceList) -> Vec<String> {
    let mut hints = Vec::new();
    match kind {
        DeviceList::Discover | DeviceList::Native => {
            let example = devices
                .iter()
                .find(|device| text(device, "transport") != "simulator" && !screen(device))
                .or_else(|| devices.first());
            if let Some(device) = example {
                hints.push(format!(
                    "Next: capturefab --camera {} capture -o 'shot-{{seq}}.png'   (or open the desktop app: capturefab)",
                    crate::hint::shell_quote(&crate::media::redact_url(text(device, "id")))
                ));
            }
            if kind == DeviceList::Discover
                && !devices.is_empty()
                && !devices
                    .iter()
                    .any(|device| matches!(text(device, "transport"), "gig_e" | "usb3"))
            {
                hints.push(
                    "No GigE/USB3 Vision cameras answered; check the network with capturefab network-scan or capturefab doctor".into(),
                );
            }
        }
        DeviceList::Onvif => {
            if let Some(device) = devices.first() {
                hints.push(format!(
                    "Next: capturefab onvif resolve {} --username USER --password PASS",
                    crate::hint::shell_quote(&crate::media::redact_url(text(device, "endpoint")))
                ));
            }
        }
        DeviceList::Scanners => {
            if let Some(device) = devices.first() {
                hints.push(format!(
                    "Next: capturefab --camera {} capture -o 'scan-{{seq}}.png'",
                    crate::hint::shell_quote(text(device, "id"))
                ));
            }
        }
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table(devices: &[Value], kind: DeviceList) -> String {
        let mut out = Vec::new();
        write_device_table(&mut out, devices, kind).unwrap();
        String::from_utf8(out).unwrap()
    }
    fn webcam() -> Value {
        json!({"id":"avfoundation:Government Cheese Camera","transport":"media","vendor":"macos",
            "model":"Government Cheese Camera","serial":"avfoundation:Government Cheese Camera",
            "address":"avfoundation:Government Cheese Camera"})
    }
    fn gige() -> Value {
        json!({"id":"gige:02007f000001","transport":"gig_e","vendor":"Capturefab",
            "model":"Virtual GigE camera","serial":"SIM7F000001","address":"192.168.1.20"})
    }
    fn simulator() -> Value {
        json!({"id":"sim:0","transport":"simulator","vendor":"Capturefab","model":"Pattern camera",
            "serial":"SIM0","address":null})
    }

    #[test]
    fn camera_columns_line_up_and_use_plain_names() {
        let output = table(&[webcam(), gige(), simulator()], DeviceList::Discover);
        let lines: Vec<_> = output.lines().collect();
        assert!(lines[0].starts_with("ID"), "{output}");
        assert!(output.contains("GigE"), "{output}");
        assert!(!output.contains("gig_e"), "{output}");
        assert!(output.contains("Webcam"), "{output}");
        assert!(output.contains("Simulator"), "{output}");
        assert!(output.contains("192.168.1.20"), "{output}");
        let model = |line: &str, name: &str| line.rfind(name).unwrap();
        let column = model(lines[0], "MODEL");
        assert_eq!(
            model(lines[1], "Government Cheese Camera"),
            column,
            "{output}"
        );
        assert_eq!(model(lines[2], "Virtual GigE camera"), column, "{output}");
        assert_eq!(model(lines[3], "Pattern camera"), column, "{output}");
        // A serial or address equal to the ID is not repeated.
        assert_eq!(lines[1].matches("Government Cheese Camera").count(), 2);
    }

    #[test]
    fn empty_columns_are_dropped_and_long_models_shortened() {
        let native = [
            json!({"id":"avfoundation:FaceTime HD Camera","name":"FaceTime HD Camera","backend":"avfoundation"}),
            json!({"id":"avfoundation:1","name":"An extraordinarily long capture device name here","backend":"avfoundation"}),
        ];
        let output = table(&native, DeviceList::Native);
        let header = output.lines().next().unwrap();
        assert_eq!(
            header,
            format!(
                "ID{}MODEL",
                " ".repeat("avfoundation:FaceTime HD Camera".len())
            )
        );
        assert!(
            !output.contains("SERIAL") && !output.contains("TRANSPORT"),
            "{output}"
        );
        assert!(
            output.contains("An extraordinarily long capture…"),
            "{output}"
        );
    }

    #[test]
    fn screens_and_scanners_are_labeled() {
        let screen = json!({"id":"avfoundation:Capture screen 0","transport":"media",
            "model":"Capture screen 0","serial":"avfoundation:Capture screen 0"});
        let scanner = json!({"id":"scanner:sane:epson2:libusb:001:004","transport":"scanner",
            "model":"Perfection V39","serial":"scanner:sane:epson2:libusb:001:004"});
        let output = table(&[screen, scanner.clone()], DeviceList::Discover);
        assert!(output.contains("Screen"), "{output}");
        assert!(output.contains("Scanner"), "{output}");
        assert!(table(&[scanner], DeviceList::Scanners).contains("Perfection V39"));
        let uuid = "E3248000-80CE-11DB-8000-3C2AF4FA0DE9";
        let ica = json!({"id":format!("scanner:ica:{uuid}"),"transport":"scanner",
            "model":"Brother MFC-L2750DW series","serial":uuid});
        let output = table(&[ica], DeviceList::Discover);
        assert_eq!(output.matches(uuid).count(), 1, "{output}");
    }

    #[test]
    fn empty_lists_say_what_to_try() {
        let discover = table(&[], DeviceList::Discover);
        assert!(discover.starts_with("No cameras found."), "{discover}");
        assert!(discover.contains("network-scan"), "{discover}");
        assert!(discover.contains("sim:0"), "{discover}");
        assert_eq!(
            table(&[], DeviceList::Scanners),
            "No document scanners found.\n"
        );
        assert!(table(&[], DeviceList::Onvif).contains("multicast"));
        assert!(next_steps(&[], DeviceList::Discover).is_empty());
    }

    #[test]
    fn onvif_rows_show_endpoint_and_name() {
        let device = json!({"endpoint":"http://192.168.1.3/onvif/device_service",
            "addresses":["http://192.168.1.3/onvif/device_service","http://10.0.0.3/onvif/device_service"],
            "scopes":["onvif://www.onvif.org/name/Front%20Door"],"uuid":"urn:uuid:1","source":"192.168.1.3:3702"});
        let output = table(std::slice::from_ref(&device), DeviceList::Onvif);
        let row = output.lines().nth(1).unwrap();
        assert!(
            row.contains("http://192.168.1.3/onvif/device_service"),
            "{output}"
        );
        assert!(row.contains("Front Door"), "{output}");
        assert!(row.contains("http://10.0.0.3"), "{output}");
        assert!(next_steps(&[device], DeviceList::Onvif)[0].contains("onvif resolve"));
    }

    #[test]
    fn next_step_prefers_a_real_camera_and_quotes_it() {
        let hints = next_steps(&[simulator(), webcam()], DeviceList::Discover);
        assert_eq!(
            hints[0],
            "Next: capturefab --camera 'avfoundation:Government Cheese Camera' capture -o 'shot-{seq}.png'   (or open the desktop app: capturefab)"
        );
        assert!(hints[1].contains("No GigE/USB3 Vision cameras answered"));
        let hints = next_steps(&[simulator(), gige()], DeviceList::Discover);
        assert!(
            hints[0].contains("--camera gige:02007f000001 "),
            "{hints:?}"
        );
        assert_eq!(hints.len(), 1, "{hints:?}");
        let hints = next_steps(&[simulator()], DeviceList::Native);
        assert!(hints[0].contains("--camera sim:0 "), "{hints:?}");
    }
}
