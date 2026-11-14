//! Closed groov expanded-input admission and address roles from the manufacturer's
//! `OptoMMP` Protocol Guide (form 1465-260805). Pure: no I/O, no defaults.
//!
//! Admission is a finite tuple table. Neither a well-formed configured type nor an
//! equality between requested and observed identities admits a tuple by itself.

/// Reported unit type, read before any nonstatus region.
pub(crate) const STATUS_UNIT_TYPE: u64 = 0xFFFF_F030_0020;
/// Powerup Clear flag; must read zero. Clearing it is an external prerequisite.
pub(crate) const STATUS_PUC: u64 = 0xFFFF_F030_0004;

const CONFIG_BASE: u64 = 0xFFFF_F010_0000;
const CONFIG_MODULE_STRIDE: u64 = 0x3000;
const CONFIG_CHANNEL_STRIDE: u64 = 0xC0;
const CONFIG_CHANNEL_TYPE: u64 = 4;

const ANALOG_BASE: u64 = 0xFFFF_F026_0000;
const DIGITAL_BASE: u64 = 0xFFFF_F01E_0000;
const DATA_MODULE_STRIDE: u64 = 0x1000;
const DATA_CHANNEL_STRIDE: u64 = 0x40;
const ANALOG_QUALITY: u64 = 0x2C;
const DIGITAL_QUALITY: u64 = 0x14;

/// Largest expanded module index the PR1/PR2 maps address (16 chassis positions).
const EPIC_MODULES: u32 = 16;
/// The ordinary input ranges never exceed this many channels on one module.
const MAX_CHANNELS: u32 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Model {
    EpicPr1,
    EpicPr2,
    RioMm1,
    RioMm2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resource {
    AnalogEu,
    DigitalState,
}

impl Model {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::EpicPr1 => "epic_pr1",
            Self::EpicPr2 => "epic_pr2",
            Self::RioMm1 => "rio_mm1",
            Self::RioMm2 => "rio_mm2",
        }
    }
    /// Status unit-type word: reported family, not unique identity or authentication.
    pub(crate) const fn unit_type(self) -> u32 {
        match self {
            Self::EpicPr1 => 0x101,
            Self::EpicPr2 => 0x107,
            Self::RioMm1 => 0x103,
            Self::RioMm2 => 0x108,
        }
    }
}

impl Resource {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::AnalogEu => "analog_eu",
            Self::DigitalState => "digital_state",
        }
    }
}

/// One inclusive range of configured channel-type IDs (not a byte-prefix test).
type Ids = &'static [(u32, u32)];

const RIO_DI_LOW: Ids = &[(0x5000_0068, 0x5000_0068), (0x5000_0079, 0x5000_0079)];
const RIO_DI_HIGH: Ids = &[(0x5400_007A, 0x5400_007A), (0x5400_0085, 0x5400_0085)];
/// Analog types documented only on universal channels 0–3.
const UNIVERSAL_AI_LOW: Ids = &[
    (0x6000_0005, 0x6000_0006),
    (0x6000_0008, 0x6000_0009),
    (0x6000_000C, 0x6000_000C),
    (0x6000_000E, 0x6000_0013),
    (0x6000_0015, 0x6000_0016),
];
/// Analog types documented on universal channels 0–7.
const UNIVERSAL_AI_ALL: Ids = &[
    (0x6000_0018, 0x6000_0018),
    (0x6000_0031, 0x6000_0031),
    (0x6000_0033, 0x6000_0033),
    (0x6000_003F, 0x6000_0043),
];

const V8: Ids = &[
    (0x6000_0001, 0x6000_0001),
    (0x6000_000A, 0x6000_000B),
    (0x6000_000D, 0x6000_000D),
    (0x6000_0017, 0x6000_0017),
    (0x6000_0019, 0x6000_0019),
    (0x6000_0024, 0x6000_0025),
];
const TC15: Ids = &[
    (0x6000_0002, 0x6000_0009),
    (0x6000_000C, 0x6000_000C),
    (0x6000_000E, 0x6000_0013),
];
const RTD30: Ids = &[(0x6000_003B, 0x6000_003D), (0x6000_0044, 0x6000_005E)];
const TH18: Ids = &[(0x6000_0032, 0x6000_0043)];

/// Ordinary EPIC module: ID, channel count, input resource and admitted types.
struct Module {
    id: u32,
    channels: u32,
    resource: Resource,
    types: Ids,
}
const fn digital(id: u32, channels: u32, types: Ids) -> Module {
    Module {
        id,
        channels,
        resource: Resource::DigitalState,
        types,
    }
}
const fn analog(id: u32, channels: u32, types: Ids) -> Module {
    Module {
        id,
        channels,
        resource: Resource::AnalogEu,
        types,
    }
}
const PLAIN_DI: Ids = &[(0x5000_0000, 0x5000_0000)];
const SIMPLE_DI: Ids = &[(0x5400_0000, 0x5400_0000)];

const EPIC_MODULES_TABLE: &[Module] = &[
    digital(0x5000_000F, 24, PLAIN_DI), // GRV-IAC-24
    digital(0x5000_0005, 24, PLAIN_DI), // GRV-IACDCTTL-24
    digital(0x5000_0010, 24, PLAIN_DI), // GRV-IACHV-24
    digital(0x5000_000C, 12, PLAIN_DI), // GRV-IACI-12
    digital(0x5000_000D, 12, PLAIN_DI), // GRV-IACIHV-12
    digital(0x5000_0011, 24, PLAIN_DI), // GRV-IDC-24
    digital(
        0x5000_000E,
        12,
        &[(0x5000_0000, 0x5000_0000), (0x5000_00A2, 0x5000_00A2)],
    ), // GRV-IDCI-12
    digital(0x5000_0023, 12, &[(0x5000_0068, 0x5000_0068)]), // GRV-IDCSW-12
    digital(0x5400_0007, 24, SIMPLE_DI), // GRV-IACDCTTLS-24
    digital(0x5400_0008, 24, SIMPLE_DI), // GRV-IACHVS-24
    digital(0x5400_0009, 12, SIMPLE_DI), // GRV-IACIHVS-12
    digital(0x5400_000A, 12, SIMPLE_DI), // GRV-IACIS-12
    digital(0x5400_001D, 24, SIMPLE_DI), // GRV-IACS-24
    digital(0x5400_000B, 12, SIMPLE_DI), // GRV-IDCIS-12
    digital(0x5400_0016, 24, SIMPLE_DI), // GRV-IDCS-24
    analog(0x6000_001F, 12, &[(0x6000_0031, 0x6000_0031)]), // GRV-IICTD-12
    analog(0x6000_0001, 24, &[(0x6000_0014, 0x6000_0016)]), // GRV-IMA-24
    analog(0x6000_0020, 8, &[(0x6000_0015, 0x6000_0016)]), // GRV-IMAI-8
    analog(0x6000_0024, 8, RTD30),      // GRV-IRTD-8
    analog(0x6000_0026, 12, TC15),      // GRV-ITM-12
    analog(0x6000_0006, 8, TC15),       // GRV-ITMI-8
    analog(0x6000_0021, 12, TH18),      // GRV-ITR-12 (ordinary types only)
    analog(0x6000_0002, 24, V8),        // GRV-IV-24
    analog(0x6000_0025, 12, V8),        // GRV-IVI-12
    analog(0x6000_0028, 10, &[(0x6000_007C, 0x6000_0084)]), // GRV-IVIRMS-10
];

/// GRV-IDCIFQ-12: quadrature-configured inputs read only as ordinary state/quality.
const IDCIFQ: u32 = 0x5000_001A;
const EPIC_UNIVERSAL: u32 = 0xF000_002B;
const RIO_MM1: u32 = 0xF000_0022;
const RIO_MM2: u32 = 0xF000_002C;

fn contains(ids: Ids, value: u32) -> bool {
    ids.iter()
        .any(|(low, high)| (*low..=*high).contains(&value))
}

/// Universal mixed-I/O channels shared by RIO MM1/MM2 and EPIC GRV-MM1001-10.
/// EPIC's universal module documents the full DI types on 0–7; RIO only on 0–1.
fn universal(channel: u32, resource: Resource, channel_type: u32, epic: bool) -> bool {
    if channel > 7 {
        // 8–9 are relay outputs.
        return false;
    }
    match resource {
        Resource::DigitalState => {
            if epic || channel <= 1 {
                contains(RIO_DI_LOW, channel_type)
            } else {
                contains(RIO_DI_HIGH, channel_type)
            }
        }
        Resource::AnalogEu => {
            contains(UNIVERSAL_AI_ALL, channel_type)
                || (channel <= 3 && contains(UNIVERSAL_AI_LOW, channel_type))
        }
    }
}

/// The single closed admission predicate.
pub(crate) fn allows_tuple(
    model: Model,
    module: u32,
    channel: u32,
    resource: Resource,
    module_id: u32,
    channel_type: u32,
) -> bool {
    match model {
        Model::RioMm1 | Model::RioMm2 => {
            let id = if model == Model::RioMm1 {
                RIO_MM1
            } else {
                RIO_MM2
            };
            module == 0 && module_id == id && universal(channel, resource, channel_type, false)
        }
        Model::EpicPr1 | Model::EpicPr2 => {
            if module >= EPIC_MODULES {
                return false;
            }
            if module_id == EPIC_UNIVERSAL {
                return universal(channel, resource, channel_type, true);
            }
            if module_id == IDCIFQ {
                let quadrature_b = matches!(channel, 2 | 5 | 8 | 11);
                return resource == Resource::DigitalState
                    && channel < 12
                    && channel_type
                        == if quadrature_b {
                            0x5000_0030
                        } else {
                            0x5000_002F
                        };
            }
            EPIC_MODULES_TABLE.iter().any(|entry| {
                entry.id == module_id
                    && entry.resource == resource
                    && channel < entry.channels
                    && contains(entry.types, channel_type)
            })
        }
    }
}

/// Upper bound used only to reject absurd selectors before table lookup.
pub(crate) const fn plausible(model: Model, module: u32, channel: u32) -> bool {
    match model {
        Model::RioMm1 | Model::RioMm2 => module == 0 && channel <= 7,
        Model::EpicPr1 | Model::EpicPr2 => module < EPIC_MODULES && channel < MAX_CHANNELS,
    }
}

/// Validated read addresses of one selected channel. Config and data regions use
/// different module and channel strides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Addresses {
    pub(crate) module_id: u64,
    pub(crate) channel_type: u64,
    pub(crate) value: u64,
    pub(crate) quality: u64,
}

pub(crate) fn addresses(
    model: Model,
    module: u32,
    channel: u32,
    resource: Resource,
) -> Option<Addresses> {
    if !plausible(model, module, channel) {
        return None;
    }
    let module = u64::from(module);
    let channel = u64::from(channel);
    let config = CONFIG_BASE
        .checked_add(module.checked_mul(CONFIG_MODULE_STRIDE)?)?
        .checked_add(channel.checked_mul(CONFIG_CHANNEL_STRIDE)?)?;
    let (base, quality) = match resource {
        Resource::AnalogEu => (ANALOG_BASE, ANALOG_QUALITY),
        Resource::DigitalState => (DIGITAL_BASE, DIGITAL_QUALITY),
    };
    let value = base
        .checked_add(module.checked_mul(DATA_MODULE_STRIDE)?)?
        .checked_add(channel.checked_mul(DATA_CHANNEL_STRIDE)?)?;
    Some(Addresses {
        module_id: config,
        channel_type: config.checked_add(CONFIG_CHANNEL_TYPE)?,
        value,
        quality: value.checked_add(quality)?,
    })
}

/// Whether an address is one of the fixed nondestructive read roles. The transport
/// refuses every other address, so a read-clear or calculate region can never be
/// requested even through a future programming error.
pub(crate) fn readable(address: u64) -> bool {
    if address == STATUS_UNIT_TYPE || address == STATUS_PUC {
        return true;
    }
    let within = |base: u64, module_stride: u64, channel_stride: u64, offsets: &[u64]| {
        let Some(offset) = address.checked_sub(base) else {
            return false;
        };
        let module = offset / module_stride;
        let rest = offset % module_stride;
        let channel = rest / channel_stride;
        let field = rest % channel_stride;
        module < u64::from(EPIC_MODULES)
            && channel < u64::from(MAX_CHANNELS)
            && offsets.contains(&field)
    };
    within(
        CONFIG_BASE,
        CONFIG_MODULE_STRIDE,
        CONFIG_CHANNEL_STRIDE,
        &[0, CONFIG_CHANNEL_TYPE],
    ) || within(
        ANALOG_BASE,
        DATA_MODULE_STRIDE,
        DATA_CHANNEL_STRIDE,
        &[0, ANALOG_QUALITY],
    ) || within(
        DIGITAL_BASE,
        DATA_MODULE_STRIDE,
        DATA_CHANNEL_STRIDE,
        &[0, DIGITAL_QUALITY],
    )
}
