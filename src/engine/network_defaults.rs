//! Apply camera-side pacing without guessing host-wide privileged settings.
use crate::{camera::Camera, network::NetworkTuning};
use anyhow::{Result, ensure};

fn read(camera: &mut Camera, names: &[&str]) -> Option<f64> {
    names.iter().find_map(|name| {
        camera
            .get(name)
            .ok()?
            .as_f64()
            .filter(|value| value.is_finite() && *value > 0.0)
    })
}

fn set_number(camera: &mut Camera, name: &str, target: f64, round_up: bool) -> Result<f64> {
    let bounds = camera.bounds(name)?;
    let min = bounds.min.unwrap_or(0.0);
    let max = bounds.max.unwrap_or(f64::MAX);
    ensure!(
        min <= max && target.is_finite(),
        "invalid bounds for {name}"
    );
    let mut value = target.clamp(min, max);
    if let Some(step) = bounds.inc.filter(|step| step.is_finite() && *step > 0.0) {
        let steps = (value - min) / step;
        value = min
            + if round_up {
                steps.ceil()
            } else {
                steps.floor()
            } * step;
    }
    // Integer nodes sometimes omit Inc; integer wire values must stay integral.
    if camera
        .get(name)
        .is_ok_and(|value| value.is_i64() || value.is_u64())
    {
        value = if round_up {
            value.ceil()
        } else {
            value.floor()
        };
    }
    ensure!(
        value >= min
            && value <= max
            && (if round_up {
                value >= target
            } else {
                value <= target
            }),
        "{name} cannot fit the available network budget"
    );
    camera.set(name, &value.to_string())?;
    let actual = camera
        .get(name)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| anyhow::anyhow!("invalid {name} readback"))?;
    ensure!(
        actual >= min && actual <= max,
        "{name} readback is outside its bounds"
    );
    ensure!(
        if round_up {
            actual >= value
        } else {
            actual <= target
        },
        "{name} readback exceeds the available network budget"
    );
    Ok(actual)
}

pub(super) fn apply(camera: &mut Camera, requested: u64) -> Result<NetworkTuning> {
    let mut tuning = camera.optimize_network()?;
    let physical = crate::network::camera_link_bytes_per_second(camera);
    if let Some(speed) = physical {
        tuning
            .changes
            .push(format!("Camera link speed: {:.0} Mb/s", speed / 125_000.0));
        if speed < 125_000_000.0 {
            tuning.warnings.push("Camera link is below Gigabit speed. Check the camera cable and switch port; bandwidth has been reduced to fit this link.".into());
        }
    }
    let budget = physical.map_or(requested, |speed| {
        requested.min((speed * 0.8).max(1.0) as u64)
    });
    camera.set_network_budget_bytes_per_second(budget);
    let mut failures = Vec::new();
    for feature in ["DeviceLinkThroughputLimit", "StreamBytesPerSecond"] {
        // On some devices this mode unlocks the limit node.
        if feature == "DeviceLinkThroughputLimit"
            && camera.has(feature)
            && camera.is_writable("DeviceLinkThroughputLimitMode")
            && let Err(error) = camera.set("DeviceLinkThroughputLimitMode", "On")
        {
            failures.push(format!(
                "Could not enable camera throughput limiter: {error:#}"
            ));
            continue;
        }
        if !camera.is_writable(feature) {
            continue;
        }
        let result = (|| -> Result<f64> {
            let actual = set_number(camera, feature, budget as f64, false)?;
            ensure!(actual > 0.0, "camera throughput readback must be positive");
            if feature == "DeviceLinkThroughputLimit" && camera.has("DeviceLinkThroughputLimitMode")
            {
                if camera.get("DeviceLinkThroughputLimitMode")? != "On" {
                    camera.set("DeviceLinkThroughputLimitMode", "On")?;
                }
                ensure!(
                    camera.get("DeviceLinkThroughputLimitMode")? == "On",
                    "camera throughput limiter is not enabled"
                );
            }
            Ok(actual)
        })();
        match result {
            Ok(actual) => {
                tuning.warnings.extend(failures);
                tuning.throughput_limit_bps = Some(actual as u64);
                tuning.changes.push(format!(
                    "{feature}: {:.1} MB/s per camera (shared link budget)",
                    actual / 1e6
                ));
                return Ok(tuning);
            }
            Err(error) => failures.push(format!("{feature}: {error:#}")),
        }
    }
    // SFNC GevSCPD is measured in timestamp ticks, whose frequency must be
    // known. Include Ethernet framing/gap overhead as well as the IP packet.
    if camera.is_writable("GevSCPD")
        && let (Some(frequency), Some(packet)) = (
            read(
                camera,
                &["GevTimestampTickFrequency", "TimestampTickFrequency"],
            ),
            tuning.packet_size,
        )
    {
        let bytes = packet as f64 + 38.0;
        // Without a camera link-speed reading, omitting serialization time
        // adds safe headroom rather than assuming the host and camera match.
        let serialization = physical.map_or(0.0, |wire| bytes / wire);
        let delay = ((bytes / budget as f64 - serialization).max(0.0) * frequency).ceil();
        match set_number(camera, "GevSCPD", delay, true) {
            Ok(actual) => {
                tuning.warnings.extend(failures);
                if physical.is_none() {
                    tuning.warnings.push("Camera link speed is unknown; conservative packet pacing includes extra headroom.".into());
                }
                tuning.throughput_limit_bps = Some(budget);
                tuning.changes.push(format!(
                    "Packet pacing: {actual:.0} ticks · {:.1} MB/s budget",
                    budget as f64 / 1e6
                ));
                return Ok(tuning);
            }
            Err(error) => failures.push(format!("GevSCPD: {error:#}")),
        }
    }
    // Average-rate fallback for free-running cameras. External trigger timing
    // belongs to the trigger source and cannot safely be changed here.
    if !camera.get("TriggerMode").is_ok_and(|mode| mode == "On")
        && let Some(payload) = read(camera, &["PayloadSize"])
    {
        for feature in ["AcquisitionFrameRate", "AcquisitionFrameRateAbs"] {
            if !camera.is_writable(feature) {
                continue;
            }
            let fps = budget as f64 / (payload * 1.05);
            let result = (|| -> Result<f64> {
                let current = read(camera, &[feature]).unwrap_or(fps);
                let actual = set_number(camera, feature, current.min(fps), false)?;
                ensure!(actual > 0.0, "frame-rate readback must be positive");
                if camera.has("AcquisitionFrameRateEnable") {
                    camera.set("AcquisitionFrameRateEnable", "true")?;
                    ensure!(
                        camera.get("AcquisitionFrameRateEnable")? == true,
                        "frame-rate limit is not enabled"
                    );
                } else if camera.has("AcquisitionFrameRateMode") {
                    camera.set("AcquisitionFrameRateMode", "On")?;
                    ensure!(
                        camera.get("AcquisitionFrameRateMode")? == "On",
                        "frame-rate limit is not enabled"
                    );
                } else {
                    anyhow::bail!("camera has no readable frame-rate limiter enable");
                }
                Ok(actual)
            })();
            match result {
                Ok(actual) => {
                    tuning.warnings.extend(failures);
                    tuning.changes.push(format!(
                        "Frame-rate limit: {actual:.2} fps within {:.1} MB/s budget",
                        budget as f64 / 1e6
                    ));
                    tuning.warnings.push("Only average frame rate could be limited; packet bursts can still congest a shared link.".into());
                    return Ok(tuning);
                }
                Err(error) => failures.push(format!("{feature}: {error:#}")),
            }
        }
    }
    tuning.warnings.push("Camera has no usable packet pacing or throughput limiter. Reduce the frame rate or trigger rate, or use a dedicated Ethernet port.".into());
    tuning.warnings.extend(failures);
    Ok(tuning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Backend, Frame, RegisterIo, Transport};
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[derive(Clone)]
    struct Fake {
        xml: String,
        memory: Arc<Mutex<Vec<u8>>>,
    }
    impl RegisterIo for Fake {
        fn read_memory(&mut self, address: u64, length: usize) -> Result<Vec<u8>> {
            Ok(self.memory.lock().unwrap()[address as usize..address as usize + length].to_vec())
        }
        fn write_memory(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.memory.lock().unwrap()[address as usize..address as usize + data.len()]
                .copy_from_slice(data);
            Ok(())
        }
    }
    impl Backend for Fake {
        fn xml(&mut self) -> Result<String> {
            Ok(self.xml.clone())
        }
        fn start(&mut self, _: usize) -> Result<()> {
            Ok(())
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn next_frame(&mut self, _: Duration) -> Result<Frame> {
            anyhow::bail!("no frame")
        }
        fn optimize_network(&mut self, _: usize) -> Result<NetworkTuning> {
            Ok(NetworkTuning {
                packet_size: Some(1500),
                ..Default::default()
            })
        }
    }
    fn int(name: &str, address: usize, bounds: &str, access: &str) -> String {
        format!(
            "<Integer Name='{name}'><pValue>{name}Reg</pValue>{bounds}</Integer><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>{access}</AccessMode></IntReg>"
        )
    }
    fn enumeration(name: &str, address: usize) -> String {
        format!(
            "<Enumeration Name='{name}'><EnumEntry Name='Off'><Value>0</Value></EnumEntry><EnumEntry Name='On'><Value>1</Value></EnumEntry><pValue>{name}Reg</pValue></Enumeration><IntReg Name='{name}Reg'><Address>{address}</Address><Length>4</Length><AccessMode>RW</AccessMode></IntReg>"
        )
    }
    fn camera(features: &str, values: &[(usize, u32)]) -> Camera {
        let fake = Fake {
            xml: format!(
                "<RegisterDescription>{}{features}</RegisterDescription>",
                int("PayloadSize", 0, "<Min>1</Min><Max>10000000</Max>", "RO")
            ),
            memory: Arc::new(Mutex::new(vec![0; 64])),
        };
        fake.memory.lock().unwrap()[..4].copy_from_slice(&1_000_000u32.to_le_bytes());
        for (address, value) in values {
            fake.memory.lock().unwrap()[*address..*address + 4]
                .copy_from_slice(&value.to_le_bytes());
        }
        let mut info = crate::transport::simulator::info();
        info.transport = Transport::GigE;
        Camera::from_backend(info, Box::new(fake)).unwrap()
    }

    #[test]
    fn throughput_shares_are_rounded_down_and_limiter_is_enabled() {
        let features = int(
            "DeviceLinkThroughputLimit",
            4,
            "<Min>1000</Min><Max>100000000</Max><Inc>1000</Inc>",
            "RW",
        ) + &enumeration("DeviceLinkThroughputLimitMode", 8);
        let mut camera = camera(&features, &[(4, 100_000_000)]);
        let tuned = apply(&mut camera, 33_333_333).unwrap();
        assert_eq!(tuned.throughput_limit_bps, Some(33_333_000));
        assert_eq!(camera.get("DeviceLinkThroughputLimitMode").unwrap(), "On");
        assert_eq!(camera.network_budget_bytes_per_second(), Some(33_333_333));
    }

    #[test]
    fn budget_never_exceeds_camera_link_even_with_faster_host() {
        let features = int(
            "DeviceLinkThroughputLimit",
            4,
            "<Min>1</Min><Max>2000000000</Max>",
            "RW",
        ) + &int("GevLinkSpeed", 8, "", "RO");
        let mut camera = camera(&features, &[(4, 1_000_000_000), (8, 1000)]);
        assert_eq!(
            apply(&mut camera, 1_000_000_000)
                .unwrap()
                .throughput_limit_bps,
            Some(100_000_000)
        );
        assert_eq!(camera.network_budget_bytes_per_second(), Some(100_000_000));
    }

    #[test]
    fn standard_device_link_speed_is_bytes_per_second() {
        let features = int(
            "DeviceLinkThroughputLimit",
            4,
            "<Min>1</Min><Max>1000000000</Max>",
            "RW",
        ) + &int("DeviceLinkSpeed", 8, "", "RO");
        let mut camera = camera(&features, &[(4, 1_000_000_000), (8, 125_000_000)]);
        assert_eq!(
            apply(&mut camera, 1_000_000_000)
                .unwrap()
                .throughput_limit_bps,
            Some(100_000_000)
        );
    }

    #[test]
    fn unknown_speed_with_known_ticks_uses_conservative_packet_delay() {
        let features = int(
            "GevSCPD",
            4,
            "<Min>0</Min><Max>10000000</Max><Inc>8</Inc>",
            "RW",
        ) + &int("GevTimestampTickFrequency", 12, "", "RO");
        let mut camera = camera(&features, &[(12, 125_000_000)]);
        let tuned = apply(&mut camera, 50_000_000).unwrap();
        let delay = camera.get("GevSCPD").unwrap().as_u64().unwrap();
        assert!(1538.0 / (delay as f64 / 125_000_000.0) <= 50_000_000.0);
        assert!(
            tuned
                .warnings
                .iter()
                .any(|warning| warning.contains("link speed is unknown"))
        );
    }

    #[test]
    fn packet_delay_is_rounded_up_with_verified_tick_frequency() {
        let features = int(
            "GevSCPD",
            4,
            "<Min>0</Min><Max>10000000</Max><Inc>8</Inc>",
            "RW",
        ) + &int("GevLinkSpeed", 8, "", "RO")
            + &int("GevTimestampTickFrequency", 12, "", "RO");
        let mut camera = camera(&features, &[(8, 1000), (12, 125_000_000)]);
        let tuned = apply(&mut camera, 50_000_000).unwrap();
        let delay = camera.get("GevSCPD").unwrap().as_u64().unwrap();
        assert_eq!(delay % 8, 0);
        let actual_rate = 1538.0 / (1538.0 / 125_000_000.0 + delay as f64 / 125_000_000.0);
        assert!(actual_rate <= 50_000_000.0);
        assert_eq!(tuned.throughput_limit_bps, Some(50_000_000));
    }

    #[test]
    fn unknown_tick_frequency_does_not_guess_packet_delay() {
        let features = int("GevSCPD", 4, "<Min>0</Min><Max>10000000</Max>", "RW");
        let mut camera = camera(&features, &[(4, 123)]);
        let tuned = apply(&mut camera, 50_000_000).unwrap();
        assert_eq!(camera.get("GevSCPD").unwrap(), 123);
        assert!(tuned.throughput_limit_bps.is_none());
        assert!(!tuned.warnings.is_empty());
    }

    #[test]
    fn external_trigger_rate_is_preserved_and_low_budget_is_reported() {
        let features = int(
            "DeviceLinkThroughputLimit",
            4,
            "<Min>10000</Min><Max>100000000</Max>",
            "RW",
        ) + &enumeration("TriggerMode", 8)
            + &int(
                "AcquisitionFrameRate",
                12,
                "<Min>1</Min><Max>1000</Max>",
                "RW",
            )
            + &enumeration("AcquisitionFrameRateMode", 16);
        let mut camera = camera(&features, &[(4, 100_000_000), (8, 1), (12, 30)]);
        let tuned = apply(&mut camera, 1000).unwrap();
        assert_eq!(
            camera.get("DeviceLinkThroughputLimit").unwrap(),
            100_000_000
        );
        assert_eq!(camera.get("AcquisitionFrameRate").unwrap(), 30);
        assert!(tuned.throughput_limit_bps.is_none());
        assert!(
            tuned
                .warnings
                .iter()
                .any(|warning| warning.contains("cannot fit"))
        );
    }
}
