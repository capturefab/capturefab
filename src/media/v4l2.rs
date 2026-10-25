//! V4L2 queries that FFmpeg's v4l2 input does not make: whether a device
//! node captures video, and the frame intervals of each format and size.

const VIDEO_CAPTURE: u32 = 0x1;
const VIDEO_CAPTURE_MPLANE: u32 = 0x1000;
const VIDEO_M2M_MPLANE: u32 = 0x4000;
const VIDEO_M2M: u32 = 0x8000;
const DEVICE_CAPS: u32 = 0x8000_0000;

/// Whether VIDIOC_QUERYCAP capabilities describe a video capture node, as
/// systemd's v4l_id decides. `capabilities` covers every node of the
/// physical device, so a UVC camera's metadata node reports VIDEO_CAPTURE
/// there; `device_caps` describes the opened node alone. Codec and ISP
/// memory-to-memory nodes are not cameras. Nodes that report both capture
/// and output stay, as v4l2loopback virtual cameras do.
pub(super) fn captures(capabilities: u32, device_caps: u32) -> bool {
    let caps = if capabilities & DEVICE_CAPS != 0 {
        device_caps
    } else {
        capabilities
    };
    caps & (VIDEO_CAPTURE | VIDEO_CAPTURE_MPLANE) != 0 && caps & (VIDEO_M2M | VIDEO_M2M_MPLANE) == 0
}

/// The lowest and highest frame rates of frame intervals given as
/// (numerator, denominator) seconds: every discrete interval, or the ends
/// of a stepwise or continuous range.
pub(super) fn frame_rates(intervals: &[(u32, u32)]) -> Option<(f64, f64)> {
    intervals
        .iter()
        .filter(|&&(numerator, denominator)| numerator > 0 && denominator > 0)
        .map(|&(numerator, denominator)| f64::from(denominator) / f64::from(numerator))
        .fold(None, |range, fps| {
            Some(range.map_or((fps, fps), |(low, high): (f64, f64)| {
                (low.min(fps), high.max(fps))
            }))
        })
}

#[cfg(target_os = "linux")]
pub(super) use device::{Formats, captures_video};

#[cfg(target_os = "linux")]
mod device {
    use std::{
        fs::{File, OpenOptions},
        io,
        os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
        path::Path,
    };

    /// `struct v4l2_capability`.
    #[repr(C)]
    #[derive(Default)]
    struct Capability {
        driver: [u8; 16],
        card: [u8; 32],
        bus_info: [u8; 32],
        version: u32,
        capabilities: u32,
        device_caps: u32,
        reserved: [u32; 3],
    }
    /// `struct v4l2_fmtdesc`.
    #[repr(C)]
    #[derive(Default)]
    struct FormatDescription {
        index: u32,
        kind: u32,
        flags: u32,
        description: [u8; 32],
        pixel_format: u32,
        mbus_code: u32,
        reserved: [u32; 3],
    }
    /// `struct v4l2_frmivalenum`, whose union holds a discrete interval or
    /// the minimum, maximum and step of a range.
    #[repr(C)]
    #[derive(Default)]
    struct FrameInterval {
        index: u32,
        pixel_format: u32,
        width: u32,
        height: u32,
        kind: u32,
        interval: [u32; 6],
        reserved: [u32; 2],
    }
    const _: () = assert!(size_of::<Capability>() == 104);
    const _: () = assert!(size_of::<FormatDescription>() == 64);
    const _: () = assert!(size_of::<FrameInterval>() == 52);
    const QUERYCAP: libc::Ioctl = libc::_IOR::<Capability>(b'V' as u32, 0);
    const ENUM_FMT: libc::Ioctl = libc::_IOWR::<FormatDescription>(b'V' as u32, 2);
    const ENUM_FRAMEINTERVALS: libc::Ioctl = libc::_IOWR::<FrameInterval>(b'V' as u32, 75);
    const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
    const BUF_TYPE_VIDEO_CAPTURE_MPLANE: u32 = 9;
    const FRMIVAL_TYPE_DISCRETE: u32 = 1;
    /// More entries than any driver lists, in case one never ends a list.
    const MAX_ENTRIES: u32 = 256;

    fn open(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
    }
    fn ioctl<T>(device: &File, request: libc::Ioctl, value: &mut T) -> io::Result<()> {
        loop {
            // SAFETY: `request` encodes the size of the repr(C) `T` it fills,
            // and `value` is exclusively borrowed for the call.
            if unsafe { libc::ioctl(device.as_raw_fd(), request, value as *mut T) } != -1 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    fn capability(device: &File) -> io::Result<Capability> {
        let mut capability = Capability::default();
        ioctl(device, QUERYCAP, &mut capability)?;
        Ok(capability)
    }
    /// Whether the node at `path` captures video. A node that cannot be
    /// opened or queried, for example without permission, is kept, so that
    /// opening it reports why.
    pub fn captures_video(path: &Path) -> bool {
        open(path)
            .and_then(|device| capability(&device))
            .map_or(true, |capability| {
                super::captures(capability.capabilities, capability.device_caps)
            })
    }

    /// A capture node's formats, for the frame intervals FFmpeg's format
    /// listing leaves out.
    pub struct Formats {
        device: File,
        /// Each format's description, as FFmpeg lists it, and fourcc.
        formats: Vec<(String, u32)>,
    }
    impl Formats {
        pub fn open(path: &Path) -> io::Result<Self> {
            let device = open(path)?;
            // FFmpeg picks the buffer type from the device-wide capabilities.
            let kind = if capability(&device)?.capabilities & super::VIDEO_CAPTURE != 0 {
                BUF_TYPE_VIDEO_CAPTURE
            } else {
                BUF_TYPE_VIDEO_CAPTURE_MPLANE
            };
            let mut formats = Vec::new();
            for index in 0..MAX_ENTRIES {
                let mut format = FormatDescription {
                    index,
                    kind,
                    ..Default::default()
                };
                if ioctl(&device, ENUM_FMT, &mut format).is_err() {
                    break;
                }
                let length = format.description.iter().position(|&b| b == 0);
                let description = &format.description[..length.unwrap_or(32)];
                formats.push((
                    String::from_utf8_lossy(description).trim().to_owned(),
                    format.pixel_format,
                ));
            }
            Ok(Self { device, formats })
        }
        /// The frame rate bounds of the format FFmpeg lists as
        /// `description`, at a size, or `None` when the driver does not
        /// enumerate frame intervals.
        pub fn frame_rates(
            &self,
            description: &str,
            width: u32,
            height: u32,
        ) -> Option<(f64, f64)> {
            let &(_, pixel_format) = self.formats.iter().find(|(name, _)| name == description)?;
            let mut intervals = Vec::new();
            for index in 0..MAX_ENTRIES {
                let mut interval = FrameInterval {
                    index,
                    pixel_format,
                    width,
                    height,
                    ..Default::default()
                };
                if ioctl(&self.device, ENUM_FRAMEINTERVALS, &mut interval).is_err() {
                    break;
                }
                let [numerator, denominator, longest, longest_denominator, ..] = interval.interval;
                intervals.push((numerator, denominator));
                if interval.kind != FRMIVAL_TYPE_DISCRETE {
                    intervals.push((longest, longest_denominator));
                    break;
                }
            }
            super::frame_rates(&intervals)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_video_capture_nodes_are_cameras() {
        // Logitech C920 on Linux 4.16+: the capture node, then the metadata
        // node uvcvideo registers beside it with the same name and the
        // device-wide capabilities.
        assert!(captures(0x84a0_0001, 0x0420_0001));
        assert!(!captures(0x84a0_0001, 0x04a0_0000));
        // A Raspberry Pi bcm2835-codec node, and a single-planar
        // memory-to-memory driver such as vim2m.
        assert!(!captures(0x8420_4000, 0x0420_4000));
        assert!(!captures(0x8420_8000, 0x0420_8000));
        // A multi-planar capture node, and a driver without device_caps.
        assert!(captures(0x8420_1000, 0x0420_1000));
        assert!(captures(0x0500_0001, 0));
        // v4l2loopback without exclusive_caps reports capture and output.
        assert!(captures(0x8520_0003, 0x0520_0003));
    }
    #[test]
    fn frame_intervals_become_frame_rate_bounds() {
        // uvcvideo's discrete intervals for a C920's 1280x720 MJPEG frames:
        // 30, 24, 20, 15, 10, 7.5 and 5 fps.
        assert_eq!(
            frame_rates(&[(1, 30), (1, 24), (1, 20), (1, 15), (1, 10), (2, 15), (1, 5)]),
            Some((5.0, 30.0))
        );
        // A stepwise range from 1/120 s to 1 s, and NTSC rates.
        assert_eq!(frame_rates(&[(1, 120), (1, 1)]), Some((1.0, 120.0)));
        let (low, high) = frame_rates(&[(1001, 30000), (1001, 60000)]).unwrap();
        assert!((low - 29.97).abs() < 0.001 && (high - 59.94).abs() < 0.001);
        // A driver that fills no interval reports no bounds.
        assert_eq!(frame_rates(&[(0, 0)]), None);
        assert_eq!(frame_rates(&[]), None);
    }
}
