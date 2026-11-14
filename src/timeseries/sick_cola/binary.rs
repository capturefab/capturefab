//! Packed `CoLa B` from SICK 8014631/1PIK §§2.1–2.4, 4.3.6–4.3.7.
//! Every channel retains independent geometry and unscaled wire values.
use super::{io_error, malformed, remaining, unsupported};
use crate::types::{Categorized, ErrorKind, Usage};
use anyhow::Result;
use serde_json::{Value, json};
use std::{io, time::Instant};

pub(super) const REQUEST: &[u8] = b"\x02\x02\x02\x02\x00\x00\x00\x0fsRN LMDscandata\x05";

const DEFAULT_FRAME: usize = 128 * 1024;
const HARD_FRAME: usize = 256 * 1024;
const DEFAULT_POINTS: usize = 8192;
const MAX_JSON: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    pub(super) max_frame_bytes: usize,
    pub(super) max_points: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_frame_bytes: DEFAULT_FRAME,
            max_points: DEFAULT_POINTS,
        }
    }
}

impl Limits {
    pub(super) fn new(frame: Option<usize>, points: Option<usize>) -> Result<Self> {
        let limits = Self {
            max_frame_bytes: frame.unwrap_or(DEFAULT_FRAME),
            max_points: points.unwrap_or(DEFAULT_POINTS),
        };
        if !(128..=HARD_FRAME).contains(&limits.max_frame_bytes)
            || !(1..=usize::from(u16::MAX)).contains(&limits.max_points)
        {
            return Err(Usage(
                "SICK CoLa model max_frame_bytes must be 128..262144 and max_points 1..65535"
                    .into(),
            )
            .into());
        }
        Ok(limits)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Model {
    Tim5xx,
    Lms1xx,
    Lms5xx,
}

impl Model {
    const fn name(self) -> &'static str {
        match self {
            Self::Tim5xx => "tim5xx",
            Self::Lms1xx => "lms1xx",
            Self::Lms5xx => "lms5xx",
        }
    }

    const fn echoes(self) -> u8 {
        match self {
            Self::Tim5xx => 1,
            Self::Lms1xx => 2,
            Self::Lms5xx => 5,
        }
    }
}

pub(super) fn read_reply(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    deadline: Instant,
    model: Model,
    limits: Limits,
) -> Result<Value> {
    let mut frame = Vec::with_capacity(4096);
    let mut buffer = [0; 4096];
    loop {
        remaining(deadline)?;
        let room = (limits.max_frame_bytes - frame.len()).min(buffer.len());
        if room == 0 {
            return Err(malformed("binary scan exceeds configured max_frame_bytes"));
        }
        let count = match read(&mut buffer[..room]) {
            Ok(0) => {
                return Err(Categorized::error(
                    ErrorKind::Disconnected,
                    "SICK CoLa peer closed during binary scan",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        frame.extend_from_slice(&buffer[..count]);
        if frame.iter().take(4).any(|byte| *byte != 2) {
            return Err(malformed("binary scan must start with four STX bytes"));
        }
        if frame.len() < 8 {
            continue;
        }
        let length = frame_length(&frame, limits)?;
        if frame.len() > length {
            return Err(malformed("extra bytes after binary scan checksum"));
        }
        if frame.len() == length {
            let value = decode_limited(&frame, model, limits)?;
            remaining(deadline)?;
            return Ok(value);
        }
    }
}

fn frame_length(frame: &[u8], limits: Limits) -> Result<usize> {
    let Some([2, 2, 2, 2, a, b, c, d]) = frame.first_chunk::<8>() else {
        return Err(malformed("invalid binary scan header"));
    };
    let payload = usize::try_from(u32::from_be_bytes([*a, *b, *c, *d]))
        .map_err(|_| malformed("binary payload length overflow"))?;
    let total = payload
        .checked_add(9)
        .filter(|length| *length <= limits.max_frame_bytes)
        .ok_or_else(|| malformed("binary scan exceeds configured max_frame_bytes"))?;
    Ok(total)
}

#[cfg(test)]
fn decode(frame: &[u8], model: Model) -> Result<Value> {
    decode_limited(frame, model, Limits::default())
}

fn decode_limited(frame: &[u8], model: Model, limits: Limits) -> Result<Value> {
    let length = frame_length(frame, limits)?;
    if frame.len() != length {
        return Err(malformed("binary scan length mismatch"));
    }
    let payload = frame
        .get(8..length - 1)
        .ok_or_else(|| malformed("missing binary scan payload"))?;
    if frame.last().copied() != Some(payload.iter().fold(0, |xor, byte| xor ^ byte)) {
        return Err(malformed("binary scan checksum mismatch"));
    }
    let body = payload
        .strip_prefix(b"sRA LMDscandata ")
        .ok_or_else(|| malformed("binary poll requires sRA LMDscandata"))?;
    let mut value = scan_body(body, model, limits, frame.len())?;
    value["encoding"] = json!("cola_b");
    value["model_profile"] = json!(model.name());
    value["raw_frame_hex"] = json!(crate::types::hex(frame));
    if serde_json::to_vec(&value)?.len() > MAX_JSON {
        return Err(malformed("binary scan exceeds one MiB JSON value budget"));
    }
    Ok(value)
}

fn scan_body(body: &[u8], model: Model, limits: Limits, raw_frame_bytes: usize) -> Result<Value> {
    // Reserve exact raw hex plus 8KiB for bounded header/tail/channel metadata.
    // Charge every sample array before constructing its JSON/vector allocation.
    let mut budget = MAX_JSON
        .checked_sub(raw_frame_bytes.saturating_mul(2).saturating_add(8192))
        .ok_or_else(|| malformed("binary raw frame exceeds JSON budget"))?;
    let mut fields = Fields(body);
    let version = fields.u16()?;
    if version > 1 {
        return Err(unsupported("unsupported binary scan version"));
    }
    let device_number = fields.u16()?;
    let serial_number = fields.u32()?;
    let device_status = fields.array::<2>()?;
    let telegram_counter = fields.u16()?;
    let scan_counter = fields.u16()?;
    let startup_us = fields.u32()?;
    let transmit_us = fields.u32()?;
    let digital_inputs_raw = fields.array::<2>()?;
    let digital_outputs_raw = fields.array::<2>()?;
    let layer_angle_raw = fields.u16()?;
    if layer_angle_raw != 0 {
        return Err(unsupported("binary profile requires a single-layer scan"));
    }
    let scan_frequency_raw = fields.u32()?;
    let measurement_frequency_raw = fields.u32()?;
    let encoder_count = fields.u16()?;
    let encoder_limit = if matches!(model, Model::Tim5xx) { 0 } else { 3 };
    if encoder_count > encoder_limit {
        return Err(unsupported("encoder count exceeds selected model profile"));
    }
    let mut encoders = Vec::with_capacity(usize::from(encoder_count));
    for _ in 0..encoder_count {
        encoders.push(json!({"position_raw":fields.u32()?,"speed_raw":fields.u16()?}));
    }
    let mut channels = Vec::new();
    for width in [16, 8] {
        let count = fields.u16()?;
        // The manual's exact count rows conflict with its RSSI settings/name
        // tables. Bound by the documented DIST/RSSI namespace, not example sets
        // such as LMS5xx's "1 or 5" which would exclude 16-bit remission.
        let limit = u16::from(model.echoes())
            * if width == 16 || matches!(model, Model::Tim5xx) {
                2
            } else {
                1
            };
        if count > limit {
            return Err(unsupported(
                "channel count exceeds selected model namespace",
            ));
        }
        for _ in 0..count {
            channels.push(channel(&mut fields, width, model, limits, &mut budget)?);
        }
    }
    if fields.u16()? != 0 {
        return Err(unsupported("binary position data is unsupported"));
    }
    let device_name = fields.text()?;
    // Current LMS1xx/LMS5xx model tables mark the comment slot reserved.
    fields.zero("comment is reserved in selected model profile")?;
    let calendar = if fields.flag()? {
        if matches!(model, Model::Tim5xx) {
            return Err(unsupported(
                "calendar is unsupported by selected model profile",
            ));
        }
        Some(calendar(&mut fields)?)
    } else {
        None
    };
    let event_count = fields.u16()?;
    if event_count > 3 || (event_count != 0 && !matches!(model, Model::Lms5xx)) {
        return Err(unsupported("event count exceeds selected model profile"));
    }
    if event_count != 0 && encoder_count != 0 {
        return Err(malformed(
            "external encoder and event data are mutually exclusive",
        ));
    }
    let mut events = Vec::with_capacity(usize::from(event_count));
    for _ in 0..event_count {
        if fields.array::<4>()? != *b"FDIN" {
            return Err(unsupported("unsupported binary event type"));
        }
        events.push(json!({"type":"FDIN","encoder_position_raw":fields.u32()?,
            "startup_us":fields.u32()?,"angle_raw":fields.i32()?}));
    }
    if !fields.0.is_empty() {
        return Err(malformed("unexpected bytes after binary scan fields"));
    }
    let mut value = json!({"version":version,"device_number":device_number,
        "serial_number":serial_number,"device_status":device_status,
        "telegram_counter":telegram_counter,"scan_counter":scan_counter,
        "startup_us":startup_us,"transmit_us":transmit_us,
        "digital_inputs_raw":digital_inputs_raw,"digital_outputs_raw":digital_outputs_raw,
        "layer_angle_raw":layer_angle_raw,"scan_frequency_raw":scan_frequency_raw,
        "measurement_frequency_raw":measurement_frequency_raw,
        "encoders":encoders,"channels":channels,"events":events});
    for (key, optional) in [
        ("device_name", device_name.map(Value::String)),
        ("calendar_raw", calendar),
    ] {
        if let Some(optional) = optional {
            value[key] = optional;
        }
    }
    Ok(value)
}

fn channel(
    fields: &mut Fields<'_>,
    width: u8,
    model: Model,
    limits: Limits,
    budget: &mut usize,
) -> Result<Value> {
    let name = fields.array::<5>()?;
    let [a, b, c, d, echo] = name;
    let distance = match [a, b, c, d] {
        [b'D', b'I', b'S', b'T'] => true,
        [b'R', b'S', b'S', b'I'] => false,
        _ => return Err(unsupported("unknown binary scan channel name")),
    };
    // Current model manuals identify only RSSI in the eight-bit section.
    // The older shared listing's eight-bit DIST rows are inconsistent with it.
    if width == 8 && distance && !matches!(model, Model::Tim5xx) {
        return Err(unsupported("eight-bit model channels require RSSI"));
    }
    if !(b'1'..=b'0' + model.echoes()).contains(&echo) {
        return Err(unsupported("channel echo exceeds selected model profile"));
    }
    let name =
        std::str::from_utf8(&name).map_err(|_| malformed("binary channel name is not ASCII"))?;
    let scale_bits = fields.u32()?;
    let offset_bits = fields.u32()?;
    let start_angle_raw = fields.i32()?;
    let angle_step_raw = fields.u16()?;
    let count = usize::from(fields.u16()?);
    if count > limits.max_points {
        return Err(malformed("binary channel exceeds configured max_points"));
    }
    let bytes_per_value = usize::from(width / 8);
    let bytes = fields.take(count * bytes_per_value)?;
    let digits = if width == 16 {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|word| digit_count(u16::from_be_bytes(*word)))
            .sum::<usize>()
    } else {
        bytes
            .iter()
            .map(|value| digit_count(u16::from(*value)))
            .sum::<usize>()
    };
    let required = digits + count.saturating_sub(1) + 2;
    *budget = budget
        .checked_sub(required)
        .ok_or_else(|| malformed("binary sample arrays exceed JSON budget"))?;
    let values = if width == 16 {
        json!(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_be_bytes(*word))
                .collect::<Vec<_>>()
        )
    } else {
        json!(bytes)
    };
    let mut value = json!({"name":name,"sample_bits":width,"scale_bits":scale_bits,
        "offset_bits":offset_bits,"start_angle_raw":start_angle_raw,
        "angle_step_raw":angle_step_raw,"values":values});
    if distance {
        // Values remain counts; the factor/offset, not these raw samples, yield mm.
        value["scaled_unit"] = json!("mm");
    }
    Ok(value)
}

const fn digit_count(value: u16) -> usize {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1000..=9999 => 4,
        _ => 5,
    }
}

fn calendar(fields: &mut Fields<'_>) -> Result<Value> {
    let year = fields.u16()?;
    let [month, day, hour, minute, second] = fields.array::<5>()?;
    let microsecond = fields.u32()?;
    // Retain documented zero month/day values and raw components. A timezone,
    // clock synchronization status and a UTC epoch are not present on the wire.
    if !(1970..=2037).contains(&year)
        || month > 12
        || day > 31
        || hour > 23
        || minute > 59
        || second > 59
        || microsecond > 999_999
    {
        return Err(malformed(
            "binary calendar component exceeds documented width/range",
        ));
    }
    Ok(json!({"year":year,"month":month,"day":day,"hour":hour,
        "minute":minute,"second":second,"microsecond":microsecond}))
}

struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn take(&mut self, count: usize) -> Result<&[u8]> {
        let (value, remaining) = self
            .0
            .split_at_checked(count)
            .ok_or_else(|| malformed("truncated binary scan field or sample array"))?;
        self.0 = remaining;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?
            .try_into()
            .map_err(|_| malformed("binary field width mismatch"))
    }

    fn u16(&mut self) -> Result<u16> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Result<i32> {
        self.array().map(i32::from_be_bytes)
    }

    fn flag(&mut self) -> Result<bool> {
        match self.u16()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(malformed("binary optional-field flag must be zero or one")),
        }
    }

    fn zero(&mut self, message: &str) -> Result<()> {
        if self.u16()? != 0 {
            return Err(unsupported(message));
        }
        Ok(())
    }

    fn text(&mut self) -> Result<Option<String>> {
        if !self.flag()? {
            return Ok(None);
        }
        let count = usize::from(self.u16()?);
        if count > 15 {
            return Err(malformed("binary device name exceeds fifteen bytes"));
        }
        let text = self.take(count)?;
        if text.iter().any(|byte| !(0x20..=0x7a).contains(byte)) {
            return Err(malformed("invalid binary device name character"));
        }
        Ok(Some(
            std::str::from_utf8(text)
                .map_err(|_| malformed("binary device name is not ASCII"))?
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest as _, Sha256};
    use std::{io::Read as _, time::Duration};

    fn hex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }

    // Unchanged SICKAG official test_server.py, a4a2b7330128..., valid sSN.
    const EMULATOR: &str = "0202020200000dd873534e204c4d447363616e646174612000000001011458560100d352d4d4f4750951f475a8e6000000000000000009c40000021c0000000144495354313f80000000000000ffff3cb00683047505190513051b0513051b051b051b0519051f051f0522052305210522051d0524052805210524051e052805240527052805260528052d052e05300532052e053005310534053105370537053f053b0535053d05370540053b054105440545053d05450549054305470548054a054d054a054c054b0551055105540557054f0557055605590556055805590562055d055f05640565056805670563056b05640572057005710571056f0574057005740573057b057f057e0588058705840583058a0581058c058c058a058f05950593059b059a059f059a059e05a405a805a005a405a805b005ad05af05b405b205bc05bc05ba05b905bf05c105c905cb05ce05d105cf05d405d205da05db05d905db05de05e605e705e605ea05f305f305f505f805f905fb0600060805fe060d060d060d061506100614061806250624062606240620061d060f061205fe05fc05fc05f705ec05e405e105e005db05d605ce05c205c005b805ae05aa05a905a6059b05a0058b0591058d05860584057705810576057505680562055b05540551054c0552054d054605400539053505340532052f05210529052105200516050c050c05040508050404ff04f704fb04f704ec04e804e904f004e704e304dc04d904df04d704ce04ca04c404c204b904be04bd04b804ae04aa04b204ac04a8049c04a404a2049f049b0496049104910494048c048a048a04800480047c047604740477046f04680472046f046a046304640462045d045f045d0457045504560458044c0450044e04530449044f04470440043e043d0439043a0432043104320430042804260425042c042304220420042204180421041804160418041d0411041a04150416041404110413040d040c040203fc03fd040403f703f703f903f203f403f203f303f603f303e703f103eb03ee03e603ea03e103de03e803e803e403df03e403dd03db03d903dc03db03dc03d303d303d203d203ca03c003c503c303c403c603bf03c203c503c003bf03ba03b903b403b903b503b403b603b103bc03b003b103b303b203b003a803ae03a903ad03ae03a703b903a303af03ab03a6039e0399039e0398039d03980395039e039a039403930396039503950394039703900391038d0392038d039203880386038b0380038b0389038803830386038403860388037b038a038403840384037e0384037d037103790377036b037103730371036e036c036c034e02c10261022c0220020001fc01ec01e901e601e301e201e401de01da01de01dd01ce01d701df01d501db01d401dc01de01d701d501d701d001d301d001d501d101c901d201d101cf01ce01ce01cf01c301cc01c801c501cb01d101c701c301c801c401ca01bc01c201c301c401bd01bd01c101c001cc01c001c001bc01b601bb01b901c001bc01bd01b901b801bb01b401bb01be01b501b801b501b601b401b501b301ba01b601bb01af01b301b001b901b201b201b201ad01ad01ad01ae01b701ac01ac01ab01b901b001b801bf01c701c801db02090230025802a903280360036a0370036b036c036d03720379037e0380038503800384038e03870381038d03910392039f03a1039703a703a5039e039e03ab03a203b003ac03ad03b103c003d803e403da03ce03ae039d039c039e03a2039a03a603a803ab03a903af03a803ad03af03b003b303ae03b903be03c103c503cb03cb03c603c803d003d103cf03d503dc03df03df03df03e703f60405040003f303e503d803cf03ca03cc03d203d003d103d303d803de03e003da03ec03e503e903ed03f203ea03ee03fc03f503fc03f803fb03ff03ff040c0407040a04110418042104320428041a0410040604030409040a0408040e041604170419041b0417041a041e04210429042a04240439042f043b043204390439043f044d0443044004470442044104400449044b044d044d044c044d045a045204570459045d0464046d04670467047004730471046d04770476047e048604840486048104880488048804850489048f0492048f0494049904a104a704a304a704ac04b204af04be04c104be04c804cc04cc04d104d204d404cd04d004d204d404e104e504e804e804f104fb04fa04fb050105010502050b0512051d051c051a051c051c051705230523052c0536053805380540054a0547054805520553055a055a057c058705ad05d90645068b06c806d006850652060b05d905b905aa05a605c1060c06cb06f60728072f073a07460743071306c9066e0639061c062b066b06f80750074706e80681065b064b0641064e0659067b069a06c106ff078e07d507ff0788073306e606c606cf06e007180764080d08580876087d088808910847081a07ef07ed084808cf0921097109840920084a07e807ad079f079307ab07fe091c093808a90846081307ee07d507dd07e708120884089a087b0857084408250831083f0856089008c308b4089f088a087d0880089008b508f5090408f508eb08ec08f5091209470964095b0952095b097509a209c209c009c309cb09e90a2b0a3b0a500a980ab70ac10aeb0b090b030b000b1b0b500b600b4f0aed0a6f0a300a210a100a140a110a070a050a0709f409d609ba099c09910981097a097b097c097809750976097509700970096d096e09690969096209680963096109620961095109580959095509540951094c09500942094709410945094309410945094209450938093a093d093c0931093a0947094b0954097d097f0978096d09610960095209590962096b0971096f096e096f096d096e096c0969096b09660962096a096a0967096b096a09620964096909630961095e096a095f0961095f095f0961096709640963095e0966095d096109660960095f095c0962096309660967096b096f0975097f03ee03ba03560343032c0325031c031f031603180311030f0300030302fb02fb02fe02f302ee02ec02ec02e302e202d902db02d902d602d202cf02c802ca02c302b902be02b802b302b602b202ae02ab02a602a302a00297029302900298028b028a0284028402820286027f0283027b027702790277026f027702690270026d0271026802660262025d025f02600268025d025a0255025c0256025a0257025c025902610257025d025e02620268026502620268026b0269026a026a026c0272026f0270000152535349313f80000000000000ffff3cb006830475fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefcefe9f0f6fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef8f4f1e3e4e6e5e4e2e4e4e5efe3e8ede7e4e7e5e6e3e6dfe6e5e6e4e0e3e6ece2e6e7e5e3e5e3e2e3dee4dde7e2e1e3e4e0dee2e1e5e4e7e6e2dfe3e2e1e0e1e7e2e2dde3e8e3e0e2e0e4e1e8e6e0dbdde4e1e5dee5e3e1e2e4e0e0e0dee0e1dfe3e8e7e2e2e1e0e2e1dcdbdfdfe0e4e6e0e1e1e3dde5dde7e5e7e2e0e1e5e6e3e1e6e2e2dfdfdfe4e2e5dfe6e3e3e3e6e2e6e4e7e6e4e7e5e6e3e9ebe5e9eae8e6e8ebe6ebe6e9e8e1e8e6e5e5e4e2e8eae7e5e7e8eaeae4ebe4eae6ebe9eeebebece8e9eaeae8ebebe9e9e9f0ebefecf0edf1eaedebeeedeaeeeeececedf0eceff2f4edefebeff1f2eaf2f0ece8f2eaf3eceff1eeedefeff2efefecedf0f4eceff5f0eef3eef0eff7f3f5f3f1f5f9f7f6f4f6f8f5f0ecf2fefefefefefdebe8edece9e8e2e4eae6e2eaefe9eee6eeede9e7edebebe8edeee9ebeeebe8edede8ebeae2e5e4e6e3e5e4e3e1e9ebe9e9e6e7ece6e2e6e3e3e4e7ebe1e4e2e4e8e7e6ede7e8ece6e3e5e4e6e5e3e4e6ebe7ecebe8e7e5e6ebeae6dbd1c7bbb4b7b6b5c6ecfefefae7d9d9e5e0e2e1e5e2e8e4e1e2e3dededbdededce2dddfdfe0d9dde0e0e0e0dbdeebfefefefefbe8e5dddfdbe2dddcdddbdce1d5d8d8d4dbdbd9d9dfdbdbdbdaded6dbd9e1d8dae0ecf7fefefeefe0dadddcdbd7dadddcddd8ddd8dbdddad8d8e0d8dddadcd5d8dcdcdee4e8f4fefefdf4e0dcdadedbdbdcd9daddd7dbd6dbd9dcd9ddd8dfd9dddadee1e2e5e9e3e8dfe6e5dfdbdcdadad9dfdbdadbdadadad9ddd9d5dce0e0e4e3e6e1e4eae5e1dfdddcd8dbdddbdfd7dbdcd7d7dde0dce0dee3e3eae8e4e1dfdbdddfdddad8dadfe3dddbdadfe1e4e8e7e5e9e2e6dcdbdfe0dae0e4e4dcdfdfe2d9f7fefefefefefef8fefefefefefefefefefefefefefcfdfefefefefefefefefefefefefefefef2e9e7fefefefefefefefefefefefefefefefef7faf4f5fafefefefefefefefcfefefefefefefefefef5edfefefefefefefefefefefefef1eafefefefefef4ebede8f4f9fbf1eae5e9eff8f7faebecedf5fbf7e6e8eef5f6f5f6fbfefefefeeae9edefe7dfd5d6bcbecfd4e2e8ecf1eff7f4efebe6e2e5e6e9e9e9e9e9e8e7e7eaece8e6ede4e8e9e8e5eaf1eceaebe8ede5f0ebeaeae6eceae9e9e7f2ece8dde3e2d4c7d8ebedeef7f6fefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef0d8fefec2c4bdbfbebfbebfbec1bcc1bdbec1bfbfbec5bfc3c6c5c2c4c5c6c5c8c6bfc3c1c4c5c7c2c7c9c5c4c5c2c6c9c7c3c2c4c5c5c3c4c8c3c3c3c6c6c4c8c6c7c3cbc7c8c5c7c8c3c3c7cacccfd5d4d1d9d0d0d2d2d2d0d1d3d3d4d2d2d0d3ced0000000000000000107b20101031f1e000e2900000045";
    const TIM: &str = "0202020200000079735241204c4d447363616e64617461200001ffffffffffff8001ffff0000ffffffff000000001234abcd0000000009c40000021c00000001444953543140000000bf800000ffff3cb00683000400000001000fffff0001525353493140000000bf800000ffff3cba068d00030001ff00000000000000000000db";
    const LMS1: &str = "02020202000000c0735241204c4d447363616e64617461200001ffffffffffff8001ffff0000ffffffff000000001234abcd0000000009c40000021c0003fffffffffffffffffffefffefffffffdfffd0002444953543140000000bf800000ffff3cb00683000400000001000fffff444953543240000000bf800000ffff3cb10684000400000001000fffff0002525353493140000000bf800000ffff3cba068d00030001ff525353493240000000bf800000ffff3cbb068e00030001ff000000000000000000003e";
    const LMS5: &str = "02020202000001a8735241204c4d447363616e64617461200001ffffffffffff8001ffff0000ffffffff000000001234abcd0000000009c40000021c0000000a444953543140000000bf800000ffff3cb00683000400000001000fffff444953543240000000bf800000ffff3cb10684000400000001000fffff444953543340000000bf800000ffff3cb20685000400000001000fffff444953543440000000bf800000ffff3cb30686000400000001000fffff444953543540000000bf800000ffff3cb40687000400000001000fffff525353493140000000bf800000ffff3cb50688000400000001000fffff525353493240000000bf800000ffff3cb60689000400000001000fffff525353493340000000bf800000ffff3cb7068a000400000001000fffff525353493440000000bf800000ffff3cb8068b000400000001000fffff525353493540000000bf800000ffff3cb9068c000400000001000fffff000000000001000541204220200000000107ea0b0a173b3a000f423f00034644494effffffffffffffff0036ee804644494efffffffefffffffe0036ee7f4644494efffffffdfffffffd0036ee7edd";

    #[test]
    fn official_emulator_common_body_is_exact_but_unsolicited_is_not_a_poll_reply() {
        let frame = hex(EMULATOR);
        assert_eq!(frame.len(), 3553);
        assert_eq!(
            crate::types::hex(Sha256::digest(&frame)),
            "1f270eaceb947d78cbbe1e6c60a3bf35bedbc59c9321ac827d09f5ccedebe8fc"
        );
        let payload = &frame[8..frame.len() - 1];
        assert_eq!(payload.iter().fold(0, |xor, byte| xor ^ byte), 0x45);
        let body = payload.strip_prefix(b"sSN LMDscandata ").unwrap();
        let value = scan_body(body, Model::Lms5xx, Limits::default(), frame.len()).unwrap();
        assert_eq!(value["version"], 0);
        assert_eq!(value["serial_number"], 18_110_550);
        assert_eq!(value["startup_us"], 4_101_310_801_u32);
        assert_eq!(value["transmit_us"], 4_101_351_654_u32);
        let [distance, rssi] = value["channels"].as_array().unwrap().as_slice() else {
            panic!("two original channels")
        };
        assert_eq!(distance["values"].as_array().unwrap().len(), 1141);
        assert_eq!(distance["values"][0], 1305);
        assert_eq!(distance["values"][1140], 624);
        assert_eq!(rssi["values"][0], 254);
        assert_eq!(rssi["values"][1140], 208);
        assert_eq!(distance["start_angle_raw"], -50_000);
        assert_eq!(distance["angle_step_raw"], 1667);
        assert_eq!(
            value["calendar_raw"],
            json!({"year":1970,"month":1,"day":1,"hour":3,"minute":31,"second":30,"microsecond":928_000})
        );
        assert!(
            decode(&frame, Model::Lms5xx)
                .unwrap_err()
                .to_string()
                .contains("requires sRA")
        );
    }

    #[test]
    fn specification_derived_profiles_preserve_all_channels_encoders_and_tails() {
        for (fixture, model, count) in [
            (TIM, Model::Tim5xx, 2),
            (LMS1, Model::Lms1xx, 4),
            (LMS5, Model::Lms5xx, 10),
        ] {
            let frame = hex(fixture);
            let value = decode(&frame, model).unwrap();
            assert_eq!(value["raw_frame_hex"], fixture);
            assert_eq!(value["device_status"], json!([128, 1]));
            assert_eq!(value["digital_inputs_raw"], json!([18, 52]));
            assert_eq!(value["digital_outputs_raw"], json!([171, 205]));
            assert_eq!(value["channels"].as_array().unwrap().len(), count);
            assert_eq!(value["channels"][0]["values"], json!([0, 1, 15, 65535]));
            assert_eq!(value["channels"][0]["scale_bits"], 0x4000_0000_u32);
            assert_eq!(value["channels"][0]["offset_bits"], 0xbf80_0000_u32);
            assert_eq!(value["channels"][0]["scaled_unit"], "mm");
            if matches!(model, Model::Lms1xx) {
                assert_eq!(value["encoders"].as_array().unwrap().len(), 3);
                assert_eq!(
                    value["encoders"][0],
                    json!({"position_raw":u32::MAX,"speed_raw":u16::MAX})
                );
            }
        }
        let value = decode(&hex(LMS5), Model::Lms5xx).unwrap();
        assert_eq!(value["channels"][9]["name"], "RSSI5");
        assert!(value["channels"][9].get("scaled_unit").is_none());
        assert_eq!(value["device_name"], "A B  ");
        assert_eq!(value["calendar_raw"]["microsecond"], 999_999);
        assert_eq!(value["events"][0]["encoder_position_raw"], u32::MAX);
        assert_eq!(value["events"][0]["angle_raw"], 3_600_000);
        assert_eq!(value["events"].as_array().unwrap().len(), 3);
        assert_eq!(value["events"][2]["startup_us"], u32::MAX - 2);
        assert_eq!(value["encoders"], json!([]));
        let value = decode(&hex(LMS1), Model::Lms1xx).unwrap();
        assert_ne!(
            value["channels"][0]["start_angle_raw"],
            value["channels"][2]["start_angle_raw"]
        );
        assert_eq!(value["channels"][2]["values"], json!([0, 1, 255]));
    }

    #[test]
    fn unchanged_printed_sra_example_has_bad_checksum_and_is_rejected() {
        // SICK8014631/1PIK printedp111. ActualpayloadXORCB, printedchecksum2B.
        let frame = hex(
            "0202020200000083735241204c4d447363616e6461746120000100010089a27f00000343034727477ba92747813b00000700000000001388000001680000000144495354313f80000000000000000186a0138800150893089508af08b308b008a408b008bf08b908ba08d008d308cf08de08eb08e308fe08ec090308fd08fd0000000000000000000000002b",
        );
        assert_eq!(
            crate::types::hex(Sha256::digest(&frame)),
            "f8f3930b77a84375df04f066f63ce72c23d0f446f7ea0d6c1a6507fffb7430bf"
        );
        assert!(
            decode(&frame, Model::Lms1xx)
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
    }

    fn revise(frame: &mut [u8], offset: usize, bytes: &[u8]) {
        frame[offset..offset + bytes.len()].copy_from_slice(bytes);
        let (body, checksum) = frame
            .split_last_mut()
            .map(|(last, rest)| (rest, last))
            .unwrap();
        *checksum = body[8..].iter().fold(0, |xor, byte| xor ^ byte);
    }

    #[test]
    fn binary_schema_lengths_model_names_and_optional_flags_are_strict() {
        for length in [0, 1, 7, 8, 23, 60, 100] {
            decode(&hex(LMS5)[..length], Model::Lms5xx).unwrap_err();
        }
        for (offset, bytes) in [
            (24, vec![0, 2]),
            (50, vec![0, 1]),
            (60, vec![0, 4]),
            (64, b"DIST6".to_vec()),
        ] {
            let mut frame = hex(LMS5);
            revise(&mut frame, offset, &bytes);
            decode(&frame, Model::Lms5xx).unwrap_err();
        }
        for (offset, bytes) in [
            (358, vec![0, 2]),
            (367, vec![0, 1]),
            (378, 1_000_000_u32.to_be_bytes().to_vec()),
            (382, vec![0, 4]),
        ] {
            let mut frame = hex(LMS5);
            revise(&mut frame, offset, &bytes);
            decode(&frame, Model::Lms5xx).unwrap_err();
        }
        let original = hex(LMS5);
        let mut both = original[..62].to_vec();
        both.extend_from_slice(&[0; 6]);
        both.extend_from_slice(&original[62..]);
        let length = u32::try_from(both.len() - 9).unwrap();
        revise(&mut both, 4, &length.to_be_bytes());
        revise(&mut both, 60, &1_u16.to_be_bytes());
        assert!(
            decode(&both, Model::Lms5xx)
                .unwrap_err()
                .to_string()
                .contains("mutually exclusive")
        );
        let mut eight_bit_distance = hex(LMS1);
        let name_offset = eight_bit_distance
            .windows(5)
            .position(|name| name == b"RSSI1")
            .unwrap();
        revise(&mut eight_bit_distance, name_offset, b"DIST1");
        assert!(
            decode(&eight_bit_distance, Model::Lms1xx)
                .unwrap_err()
                .to_string()
                .contains("eight-bit")
        );
        decode(&hex(LMS1), Model::Tim5xx).unwrap_err();
        decode(&hex(LMS5), Model::Lms1xx).unwrap_err();
        let mut trailing = hex(TIM);
        trailing.push(0);
        decode(&trailing, Model::Tim5xx).unwrap_err();
        for length in [0_u32, 131_064, u32::MAX] {
            let mut frame = hex(TIM);
            revise(&mut frame, 4, &length.to_be_bytes());
            decode(&frame, Model::Tim5xx).unwrap_err();
        }
        let mut bad_flag = hex(TIM);
        let offset = bad_flag.len() - 11;
        revise(&mut bad_flag, offset, &[0, 2]);
        decode(&bad_flag, Model::Tim5xx).unwrap_err();
    }

    fn long_scan(points: u16) -> Vec<u8> {
        // Conservative 4561-point/five-echo high-resolution stress fixture.
        // Current LMS5xx PRO supports190deg at0.042deg (wire step417), unlike
        // the older common listing's0.1667deg minimum. This is spec-derived.
        let header = hex(LMS5);
        let mut payload = header[8..62].to_vec();
        payload.extend_from_slice(&10_u16.to_be_bytes());
        for prefix in [b"DIST", b"RSSI"] {
            for echo in b'1'..=b'5' {
                payload.extend_from_slice(prefix);
                payload.push(echo);
                payload.extend_from_slice(&0x4000_0000_u32.to_be_bytes());
                payload.extend_from_slice(&0_u32.to_be_bytes());
                payload.extend_from_slice(&(-50_000_i32).to_be_bytes());
                payload.extend_from_slice(&417_u16.to_be_bytes());
                payload.extend_from_slice(&points.to_be_bytes());
                for _ in 0..points {
                    payload.extend_from_slice(&u16::MAX.to_be_bytes());
                }
            }
        }
        payload.extend_from_slice(&[0; 12]);
        let mut frame = vec![2; 4];
        frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&payload);
        frame.push(payload.iter().fold(0, |xor, byte| xor ^ byte));
        frame
    }

    #[test]
    fn high_resolution_all_echoes_exceed_old_limits_and_fit_documented_defaults() {
        let frame = long_scan(4561);
        assert!(frame.len() > 65_536);
        let value = decode(&frame, Model::Lms5xx).unwrap();
        assert_eq!(value["channels"].as_array().unwrap().len(), 10);
        for channel in value["channels"].as_array().unwrap() {
            assert_eq!(channel["values"].as_array().unwrap().len(), 4561);
            assert_eq!(channel["values"][4560], u16::MAX);
        }
        assert!(serde_json::to_vec(&value).unwrap().len() < MAX_JSON);
        assert!(
            decode_limited(
                &frame,
                Model::Lms5xx,
                Limits::new(Some(65_536), None).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("max_frame_bytes")
        );
        assert!(
            decode_limited(
                &frame,
                Model::Lms5xx,
                Limits::new(None, Some(4096)).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("max_points")
        );
        // Raw hex is charged before every decoded sample array allocation.
        let large = long_scan(12_000);
        assert!(large.len() < HARD_FRAME);
        assert!(
            decode_limited(
                &large,
                Model::Lms5xx,
                Limits::new(Some(HARD_FRAME), Some(12_000)).unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("JSON budget")
        );
    }

    #[test]
    fn binary_limits_reject_invalid_ranges() {
        for (frame, points) in [
            (127, 1),
            (HARD_FRAME + 1, 1),
            (DEFAULT_FRAME, 0),
            (DEFAULT_FRAME, 65_536),
        ] {
            Limits::new(Some(frame), Some(points)).unwrap_err();
        }
    }

    #[test]
    fn binary_framing_reassembles_every_byte_and_checks_deadline_and_trailing_data() {
        let frame = hex(LMS1);
        let mut input = io::Cursor::new(&frame);
        let value = read_reply(
            |buffer| input.read(&mut buffer[..1]),
            Instant::now() + Duration::from_secs(1),
            Model::Lms1xx,
            Limits::default(),
        )
        .unwrap();
        assert_eq!(value["raw_frame_hex"], LMS1);
        let mut calls = 0;
        read_reply(
            |_| {
                calls += 1;
                Ok(0)
            },
            Instant::now(),
            Model::Lms1xx,
            Limits::default(),
        )
        .unwrap_err();
        assert_eq!(calls, 0);
        let mut two = frame.clone();
        two.extend_from_slice(&frame);
        let mut input = io::Cursor::new(two);
        assert!(
            read_reply(
                |buffer| input.read(buffer),
                Instant::now() + Duration::from_secs(1),
                Model::Lms1xx,
                Limits::default()
            )
            .unwrap_err()
            .to_string()
            .contains("extra bytes")
        );
        let mut input = io::Cursor::new(&frame[..10]);
        assert!(
            read_reply(
                |buffer| input.read(buffer),
                Instant::now() + Duration::from_secs(1),
                Model::Lms1xx,
                Limits::default()
            )
            .unwrap_err()
            .to_string()
            .contains("closed")
        );
    }
}
