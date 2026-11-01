//! Which camera features and commands tools may touch, and at what risk.
//!
//! Capturefab owns a camera's transport, network, persistence and file
//! features: tools never write them, `get_camera_features` hides them, and
//! `set_camera_features` refuses them ("Capturefab manages <name>; change it
//! in the camera inspector."). Image-quality features are Reversible
//! changes; any other writable feature is Disruptive.

pub use crate::setup::{image_quality, managed, managed_name, not_a_value};

/// Commands `run_camera_command` may execute, besides `*AutoOnce` ones.
const COMMANDS: [&str; 2] = ["TriggerSoftware", "TimestampLatch"];

/// `run_camera_command` may execute this command: a software trigger, a
/// timestamp latch, or a one-shot auto adjustment such as
/// `BalanceWhiteAutoOnce`. Resets, user sets, files, firmware and
/// transport commands never qualify.
pub fn command_allowed(name: &str) -> bool {
    !managed_name(name)
        && (COMMANDS.contains(&name)
            || (name.ends_with("AutoOnce") && name.len() > "AutoOnce".len())
            || (name.starts_with("Auto") && name.ends_with("Once") && name.len() > 8))
}

/// AcquisitionStart and AcquisitionStop bypass the session's live view
/// state; tools map them to start_live_view and stop_live_view instead.
pub fn acquisition_command(name: &str) -> bool {
    matches!(name, "AcquisitionStart" | "AcquisitionStop")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_persistence_and_file_features_are_managed() {
        for name in [
            "GevSCDA",
            "GevSCPHostPort",
            "GevHeartbeatTimeout",
            "GevPersistentIPAddress",
            "TLParamsLocked",
            "DeviceLinkThroughputLimit",
            "DeviceReset",
            "DeviceFactoryReset",
            "DeviceFirmwareVersion",
            "DeviceUserID",
            "FileAccessControl",
            "FileOperationExecute",
            "UserSetSave",
            "UserSetDefault",
            "PersistentIPAddress",
            "TransferStart",
            "ChunkModeActive",
            "EventNotification",
            "DeviceStreamChannelPacketSize",
            "DeviceStreamChannelSelector",
            "DeviceStreamChannelEndianness",
        ] {
            assert!(managed(name, None, Some("Integer")), "{name}");
            assert!(!image_quality(name), "{name}");
            assert!(!command_allowed(name), "{name}");
        }
        assert!(managed("Anything", Some("FileAccessControl"), None));
        assert!(managed("Anything", Some("UserSetControl"), None));
        assert!(managed("AnyReg", None, Some("IntReg")));
        assert!(managed("TriggerSoftware", None, Some("Command")));
        assert!(!managed(
            "ExposureTime",
            Some("AcquisitionControl"),
            Some("Float")
        ));
        // Device information other than the managed families stays readable.
        assert!(!managed_name("DeviceModelName") && !managed_name("DeviceTemperature"));
    }

    #[test]
    fn image_quality_features_are_reversible_and_unknown_ones_are_not() {
        for name in [
            "ExposureTime",
            "ExposureTimeAbs",
            "Gain",
            "BalanceRatio",
            "BalanceRatioSelector",
            "FocusPosition",
            "LensPosition",
            "BinningHorizontal",
            "DecimationVertical",
            "ZoomLevel",
            "IrisAperture",
            "AcquisitionFrameRateEnable",
            "GainRaw",
            "AcquisitionFrameRateAbs",
            "AcquisitionFrameRateMode",
            "VideoMode",
            "Contrast",
        ] {
            assert!(image_quality(name), "{name}");
        }
        for name in [
            "Foo",
            "TriggerMode",
            "LineSelector",
            "DeviceTemperature",
            "",
        ] {
            assert!(!image_quality(name), "{name}");
        }
    }

    #[test]
    fn only_triggers_latches_and_one_shot_autos_run() {
        for name in [
            "TriggerSoftware",
            "TimestampLatch",
            "BalanceWhiteAutoOnce",
            "FocusAutoOnce",
            "AutoFocusOnce",
        ] {
            assert!(command_allowed(name), "{name}");
        }
        for name in [
            "DeviceReset",
            "UserSetSave",
            "UserSetLoad",
            "FileOperationExecute",
            "AcquisitionStart",
            "AutoOnce",
            "GevTimestampControlLatch",
            "TimestampReset",
            "Unknown",
        ] {
            assert!(!command_allowed(name), "{name}");
        }
        assert!(acquisition_command("AcquisitionStop") && !acquisition_command("TriggerSoftware"));
    }
}
