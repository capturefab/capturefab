//! Which camera features and commands tools may touch, and at what risk.
//!
//! Capturefab owns a camera's transport, network, persistence and file
//! features: tools never write them, `get_camera_features` hides them, and
//! `set_camera_features` refuses them ("Capturefab manages <name>; change it
//! in the camera inspector."). Image-quality features are Reversible
//! changes; any other writable feature is Disruptive.

/// Name prefixes of features Capturefab manages: GigE and transport-layer
/// settings (writing GevSCDA would send the stream elsewhere), device
/// resets and firmware, user sets and persistent settings that survive a
/// reboot, camera file access, events and chunk data.
const MANAGED_PREFIXES: [&str; 16] = [
    "Gev",
    "TL",
    "DeviceLink",
    "DeviceTL",
    "DeviceReset",
    "DeviceFactoryReset",
    "DeviceFirmware",
    "DeviceRegisters",
    "DeviceUserID",
    "File",
    "UserSet",
    "Persistent",
    "LinkCommand",
    "Transfer",
    "EventNotification",
    "Chunk",
];

/// SFNC categories of the same features, for cameras that report them.
const MANAGED_CATEGORIES: [&str; 3] = [
    "TransportLayerControl",
    "FileAccessControl",
    "UserSetControl",
];

/// Features that change only how the image looks, frames or is sized:
/// undo restores them, so tools treat writing them as Reversible.
const IMAGE_QUALITY: [&str; 30] = [
    "ExposureTime",
    "ExposureTimeAbs",
    "ExposureAuto",
    "Gain",
    "GainAbs",
    "GainAuto",
    "GainSelector",
    "BlackLevel",
    "Gamma",
    "BalanceRatio",
    "BalanceRatioSelector",
    "BalanceWhiteAuto",
    "AcquisitionFrameRate",
    "AcquisitionFrameRateEnable",
    "LensPosition",
    "Width",
    "Height",
    "OffsetX",
    "OffsetY",
    "ReverseX",
    "ReverseY",
    "PixelFormat",
    "VideoMode",
    "IlluminationIntensity",
    "Magnification",
    "Sharpness",
    "Saturation",
    "Hue",
    "Brightness",
    "Contrast",
];

/// Families of image-quality features, by name prefix.
const IMAGE_QUALITY_PREFIXES: [&str; 5] = ["Focus", "Iris", "Zoom", "Binning", "Decimation"];

/// Commands `run_camera_command` may execute, besides `*AutoOnce` ones.
const COMMANDS: [&str; 2] = ["TriggerSoftware", "TimestampLatch"];

/// Capturefab manages this feature: by name, by its SFNC category when the
/// camera reports one, or because it is a raw register or command node
/// rather than a value.
pub fn managed(name: &str, category: Option<&str>, kind: Option<&str>) -> bool {
    managed_name(name)
        || category.is_some_and(|category| MANAGED_CATEGORIES.contains(&category))
        || kind.is_some_and(not_a_value)
}

/// The feature name alone marks it as managed by Capturefab.
pub fn managed_name(name: &str) -> bool {
    MANAGED_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

/// Register (`IntReg`, `StringReg`, …) and command nodes hold no value a
/// tool may set.
pub fn not_a_value(kind: &str) -> bool {
    kind == "Command" || kind.ends_with("Reg")
}

/// Writing this feature only changes how the image looks, frames or is
/// sized, so it is a Reversible change. Unknown features are not.
pub fn image_quality(name: &str) -> bool {
    !managed_name(name)
        && (IMAGE_QUALITY.contains(&name)
            || IMAGE_QUALITY_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix)))
}

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
