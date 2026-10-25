//! V4L2 queries that FFmpeg's v4l2 input does not make: whether a device
//! node captures video.

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

#[cfg(target_os = "linux")]
pub(super) use device::captures_video;

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
    const _: () = assert!(size_of::<Capability>() == 104);
    const QUERYCAP: libc::Ioctl = libc::_IOR::<Capability>(b'V' as u32, 0);

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
    /// Whether the node at `path` captures video. A node that cannot be
    /// opened or queried, for example without permission, is kept, so that
    /// opening it reports why.
    pub fn captures_video(path: &Path) -> bool {
        let mut capability = Capability::default();
        open(path)
            .and_then(|device| ioctl(&device, QUERYCAP, &mut capability))
            .map_or(true, |()| {
                super::captures(capability.capabilities, capability.device_caps)
            })
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
}
