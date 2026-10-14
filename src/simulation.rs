//! Repeatable test cameras, exposed through the same protocols as physical cameras.
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use serde_json::{Value, json};
use std::{net::Ipv4Addr, path::PathBuf, time::Duration};

pub mod server;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Lossless RGB GigE Vision camera for industrial acquisition.
    Gige,
    /// 1080p H.264 stream for NVR recording and computer vision ingestion.
    Rtsp,
    /// Discoverable 720p ONVIF camera with an H.264 RTSP stream.
    Onvif,
    /// Lossless RGB GigE Vision at 60 fps for computer vision pipelines.
    Vision,
}

impl Preset {
    fn defaults(self) -> (u32, u32, f64, &'static str) {
        match self {
            Self::Gige => (640, 480, 30.0, "4M"),
            Self::Rtsp => (1920, 1080, 30.0, "4M"),
            Self::Onvif => (1280, 720, 15.0, "2M"),
            Self::Vision => (640, 480, 60.0, "4M"),
        }
    }
}

pub fn presets() -> Value {
    json!({"presets": [
        {"name":"gige","protocol":"GigE Vision","width":640,"height":480,"fps":30,"pixel_format":"RGB8","purpose":"Lossless industrial camera acquisition and GenICam control"},
        {"name":"rtsp","protocol":"RTSP","width":1920,"height":1080,"fps":30,"codec":"H.264","bitrate":"4M","purpose":"NVR recording, reconnects and computer vision stream ingestion"},
        {"name":"onvif","protocol":"ONVIF + RTSP","width":1280,"height":720,"fps":15,"codec":"H.264","bitrate":"2M","purpose":"Camera discovery, media profile resolution and NVR onboarding"},
        {"name":"vision","protocol":"GigE Vision","width":640,"height":480,"fps":60,"pixel_format":"RGB8","purpose":"Lossless RGB test data for computer vision pipelines"}
    ]})
}

#[derive(Debug, Subcommand)]
pub enum SimulationCommand {
    /// Show camera presets and their integration use cases.
    Presets,
    /// Host a virtual camera until Ctrl-C (or --duration).
    Run(RunOptions),
}

#[derive(Debug, Args)]
pub struct RunOptions {
    #[arg(long, value_enum, default_value = "onvif")]
    pub preset: Preset,
    /// Video file, still image, or one-level directory of images/videos; loops by filename.
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Interface to listen on; use a LAN IP for another machine's NVR.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: Ipv4Addr,
    /// Reachable camera IP in discovery and URLs; required for --bind 0.0.0.0.
    #[arg(long)]
    pub advertise: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 3956)]
    pub gige_port: u16,
    #[arg(long, default_value_t = 8554)]
    pub rtsp_port: u16,
    #[arg(long, default_value_t = 8080)]
    pub onvif_port: u16,
    #[arg(long, default_value = "Capturefab virtual camera")]
    pub name: String,
    #[arg(long, value_parser = clap::value_parser!(u32).range(64..=1920))]
    pub width: Option<u32>,
    #[arg(long, value_parser = clap::value_parser!(u32).range(64..=1080))]
    pub height: Option<u32>,
    /// Delivered camera frame rate (1..120); video timing is resampled to this rate.
    #[arg(long)]
    pub fps: Option<f64>,
    /// H.264 bitrate, e.g. 2M or 800k.
    #[arg(long)]
    pub bitrate: Option<String>,
    /// Stop automatically after a duration such as 30s or 5m.
    #[arg(long)]
    pub duration: Option<String>,
}

impl RunOptions {
    pub fn validate(&self) -> Result<()> {
        let (width, height, fps, _) = self.settings();
        ensure!(
            (64..=1920).contains(&width) && (64..=1080).contains(&height),
            "simulation dimensions must be 64..1920 by 64..1080"
        );
        ensure!(
            fps.is_finite() && (1.0..=120.0).contains(&fps),
            "simulation fps must be a finite number within 1..120"
        );
        ensure!(
            width.is_multiple_of(2) && height.is_multiple_of(2)
                || matches!(self.preset, Preset::Gige | Preset::Vision),
            "H.264 simulation dimensions must be even"
        );
        ensure!(
            !self.bind.is_multicast() && self.bind != Ipv4Addr::BROADCAST,
            "simulation bind must be a local unicast IP or 0.0.0.0"
        );
        let advertise = self.advertised_ip()?;
        ensure!(
            !advertise.is_unspecified()
                && !advertise.is_multicast()
                && advertise != Ipv4Addr::BROADCAST,
            "simulation advertise must be a reachable unicast IP"
        );
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "simulation name must contain 1..128 bytes without control characters"
        );
        if matches!(self.preset, Preset::Onvif)
            && self.rtsp_port != 0
            && self.rtsp_port == self.onvif_port
        {
            anyhow::bail!("RTSP and ONVIF need different listening ports");
        }
        if let Some(source) = &self.source {
            ensure!(
                !source.as_os_str().is_empty(),
                "simulation source path must be nonempty"
            );
            ensure!(
                source
                    .to_str()
                    .is_some_and(|s| !s.chars().any(char::is_control)),
                "simulation source path must be UTF-8 without control characters"
            );
        }
        if let Some(duration) = &self.duration {
            crate::scheduling::parse_duration(duration)?;
        }
        let bitrate = self.bitrate.as_deref().unwrap_or(self.preset.defaults().3);
        ensure!(
            valid_bitrate(bitrate),
            "simulation bitrate must be a positive number optionally followed by k or M, for example 2M"
        );
        Ok(())
    }

    fn settings(&self) -> (u32, u32, f64, &str) {
        let (width, height, fps, bitrate) = self.preset.defaults();
        (
            self.width.unwrap_or(width),
            self.height.unwrap_or(height),
            self.fps.unwrap_or(fps),
            self.bitrate.as_deref().unwrap_or(bitrate),
        )
    }

    fn advertised_ip(&self) -> Result<Ipv4Addr> {
        self.advertise
            .or((!self.bind.is_unspecified()).then_some(self.bind))
            .context("--bind 0.0.0.0 requires --advertise with the camera's reachable LAN IP")
    }

    pub fn start(&self, timeout: Duration) -> Result<server::VirtualCameraServer> {
        self.validate()?;
        let (width, height, fps, bitrate) = self.settings();
        let mut camera = if let Some(source) = &self.source {
            let selector = format!(
                "sim:replay:{}",
                source.to_str().context("simulation source must be UTF-8")?
            );
            let backend =
                crate::transport::simulator::replay::open(source, width, height, fps, timeout)?;
            crate::camera::Camera::from_backend(
                crate::transport::simulator::info_named(&selector),
                backend,
            )?
        } else {
            let mut camera = crate::camera::Camera::open("sim:0", &[], timeout)?;
            camera.set("Width", &width.to_string())?;
            camera.set("Height", &height.to_string())?;
            camera.set("PixelFormat", "RGB8")?;
            camera.set("AcquisitionFrameRate", &fps.to_string())?;
            camera
        };
        // Expose exactly the configured format before listeners report readiness.
        ensure!(
            camera.get("PayloadSize")?.as_u64().is_some_and(|n| n > 0),
            "simulation source has no image payload"
        );
        let gige = matches!(self.preset, Preset::Gige | Preset::Vision);
        server::VirtualCameraServer::start(
            camera,
            server::ServerOptions {
                bind: self.bind,
                advertise: self.advertised_ip()?,
                gige_port: gige.then_some(self.gige_port),
                rtsp_port: (!gige).then_some(self.rtsp_port),
                onvif_port: (self.preset == Preset::Onvif).then_some(self.onvif_port),
                name: self.name.clone(),
                fps,
                bitrate: bitrate.to_owned(),
            },
        )
    }
}

fn valid_bitrate(value: &str) -> bool {
    let number = value.strip_suffix(['k', 'K', 'm', 'M']).unwrap_or(value);
    !number.is_empty()
        && number.len() <= 9
        && number.bytes().all(|b| b.is_ascii_digit())
        && number.parse::<u32>().is_ok_and(|n| n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn options(args: &[&str]) -> RunOptions {
        let cli = crate::cli::Cli::try_parse_from(
            [&["capturefab", "simulate", "run"][..], args].concat(),
        )
        .unwrap();
        match cli.command.unwrap() {
            crate::cli::Command::Simulate {
                command: SimulationCommand::Run(options),
            } => options,
            _ => unreachable!(),
        }
    }

    #[test]
    fn presets_have_useful_defaults_and_validate_without_network_access() {
        for (preset, size, fps) in [
            ("gige", (640, 480), 30.0),
            ("rtsp", (1920, 1080), 30.0),
            ("onvif", (1280, 720), 15.0),
            ("vision", (640, 480), 60.0),
        ] {
            let options = options(&["--preset", preset]);
            options.validate().unwrap();
            let (width, height, rate, _) = options.settings();
            assert_eq!((width, height), size);
            assert_eq!(rate, fps);
        }
    }

    #[test]
    fn invalid_camera_simulation_options_fail_before_opening_a_source() {
        for args in [
            vec!["--fps", "NaN"],
            vec!["--fps", "0"],
            vec!["--bind", "0.0.0.0"],
            vec!["--advertise", "0.0.0.0"],
            vec!["--bitrate", "-1"],
            vec!["--width", "641"],
            vec!["--duration", "0s"],
            vec!["--rtsp-port", "8080"],
            vec!["--name", ""],
        ] {
            // clap itself may reject malformed values before semantic validation.
            if let Ok(cli) = crate::cli::Cli::try_parse_from(
                [&["capturefab", "simulate", "run"][..], args.as_slice()].concat(),
            ) && let Some(crate::cli::Command::Simulate {
                command: SimulationCommand::Run(options),
            }) = cli.command
            {
                assert!(options.validate().is_err(), "{args:?}");
            }
        }
        options(&["--bind", "0.0.0.0", "--advertise", "192.0.2.10"])
            .validate()
            .unwrap();
    }
}
