//! Baseline SIG profile helpers (CORE-PROFILES, UBM 5.0).
//!
//! Pure-codec mirror of the retained `src/profiles/**` oracle plus thin
//! selectors over [`central::Central`](crate::central::Central). Codecs fail
//! closed with [`ProfileCodecError`] (same `profile.codec.*` identities as
//! the TypeScript oracle); path selection goes through the real
//! [`central::Central::resolve_path`] so there is no second lifecycle, no
//! reconnect loop, and no stub backend. The caller owns connect/discover;
//! this module only resolves a characteristic path and decodes its bytes.
//!
//! Acceptance links: `DATA-03`, `GATT-04`, `STR-01`, `PKG-03`.

use crate::central::{Central, PathSelector, canonical_uuid};
use crate::contracts::{BleErrorCode, BleErrorDomain, CoreError};

/// Canonical 128-bit SIG service/characteristic UUIDs.
pub const HEART_RATE_SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
pub const HEART_RATE_MEASUREMENT_CHARACTERISTIC: &str = "00002a37-0000-1000-8000-00805f9b34fb";
pub const BODY_SENSOR_LOCATION_CHARACTERISTIC: &str = "00002a38-0000-1000-8000-00805f9b34fb";
pub const HEART_RATE_CONTROL_POINT_CHARACTERISTIC: &str = "00002a39-0000-1000-8000-00805f9b34fb";
pub const BATTERY_SERVICE: &str = "0000180f-0000-1000-8000-00805f9b34fb";
pub const BATTERY_LEVEL_CHARACTERISTIC: &str = "00002a19-0000-1000-8000-00805f9b34fb";

/// Standards-level payload failure, mirroring `ProfileCodecError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileCodecError {
    code: ProfileCodecCode,
    codec: &'static str,
    detail: String,
    offset: Option<usize>,
}

/// Codec failure identity (wire strings match the TypeScript oracle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileCodecCode {
    Truncated,
    Malformed,
    Reserved,
    InvalidValue,
}

impl ProfileCodecCode {
    /// Frozen wire string for this code.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Truncated => "profile.codec.truncated",
            Self::Malformed => "profile.codec.malformed",
            Self::Reserved => "profile.codec.reserved",
            Self::InvalidValue => "profile.codec.invalid-value",
        }
    }
}

impl ProfileCodecError {
    /// Borrow the failure identity.
    #[must_use]
    pub const fn code(&self) -> ProfileCodecCode {
        self.code
    }

    /// Borrow the codec name.
    #[must_use]
    pub const fn codec(&self) -> &'static str {
        self.codec
    }

    /// Borrow the human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// Borrow the byte offset, when the failure is positional.
    #[must_use]
    pub const fn offset(&self) -> Option<usize> {
        self.offset
    }
}

fn codec_error(
    code: ProfileCodecCode,
    codec: &'static str,
    detail: String,
    offset: Option<usize>,
) -> ProfileCodecError {
    ProfileCodecError {
        code,
        codec,
        detail,
        offset,
    }
}

fn require_exact_length(
    bytes: &[u8],
    expected: usize,
    codec: &'static str,
) -> Result<(), ProfileCodecError> {
    if bytes.len() != expected {
        let mut detail = String::from("requires exactly ");
        append_usize(&mut detail, expected);
        detail.push_str(" bytes; received ");
        append_usize(&mut detail, bytes.len());
        return Err(codec_error(
            ProfileCodecCode::Malformed,
            codec,
            detail,
            None,
        ));
    }
    Ok(())
}

fn require_remaining(
    bytes: &[u8],
    offset: usize,
    required: usize,
    codec: &'static str,
) -> Result<(), ProfileCodecError> {
    if offset.saturating_add(required) > bytes.len() {
        let mut detail = String::from("requires ");
        append_usize(&mut detail, required);
        detail.push_str(" bytes at offset ");
        append_usize(&mut detail, offset);
        detail.push_str("; received ");
        append_usize(&mut detail, bytes.len().saturating_sub(offset));
        return Err(codec_error(
            ProfileCodecCode::Truncated,
            codec,
            detail,
            Some(offset),
        ));
    }
    Ok(())
}

fn read_u8(bytes: &[u8], offset: usize, codec: &'static str) -> Result<u8, ProfileCodecError> {
    require_remaining(bytes, offset, 1, codec)?;
    Ok(bytes[offset])
}

fn read_u16_le(bytes: &[u8], offset: usize, codec: &'static str) -> Result<u16, ProfileCodecError> {
    require_remaining(bytes, offset, 2, codec)?;
    Ok(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
}

fn read_u32_le(bytes: &[u8], offset: usize, codec: &'static str) -> Result<u32, ProfileCodecError> {
    require_remaining(bytes, offset, 4, codec)?;
    Ok(u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]))
}

fn append_usize(into: &mut String, mut value: usize) {
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits: [u8; 20] = [0; 20];
    let mut count = 0usize;
    while value > 0 && count < digits.len() {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    while count > 0 {
        count -= 1;
        into.push(digits[count] as char);
    }
}

fn assert_no_reserved_flag_bits(
    flags: u8,
    allowed_mask: u8,
    codec: &'static str,
) -> Result<(), ProfileCodecError> {
    let reserved = flags & !allowed_mask;
    if reserved != 0 {
        let mut detail = String::from("reserved flag bits are set: 0x");
        append_hex_byte(&mut detail, reserved);
        return Err(codec_error(ProfileCodecCode::Reserved, codec, detail, None));
    }
    Ok(())
}

fn append_hex_byte(into: &mut String, value: u8) {
    append_hex_u32(into, u32::from(value));
}

/// Parses Battery Level (0x2A19) as its mandatory UINT8 percentage.
pub fn parse_battery_level(bytes: &[u8]) -> Result<u8, ProfileCodecError> {
    const CODEC: &str = "Battery Level";
    require_exact_length(bytes, 1, CODEC)?;
    let percent = read_u8(bytes, 0, CODEC)?;
    if percent > 100 {
        let mut detail = String::from("percentage ");
        append_usize(&mut detail, usize::from(percent));
        detail.push_str(" is outside 0 through 100");
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            CODEC,
            detail,
            None,
        ));
    }
    Ok(percent)
}

/// Encodes Battery Level (0x2A19); values outside 0-100 fail closed.
pub fn encode_battery_level(percent: u8) -> Result<[u8; 1], ProfileCodecError> {
    const CODEC: &str = "Battery Level";
    if percent > 100 {
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            CODEC,
            String::from("percentage must be an integer from 0 through 100"),
            None,
        ));
    }
    Ok([percent])
}

/// Decoded IEEE-11073 20601 FLOAT/SFLOAT value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ieee11073Value {
    Finite {
        mantissa: i32,
        exponent: i8,
        value: f64,
    },
    Nan,
    Nres,
    PositiveInfinity,
    NegativeInfinity,
}

const FLOAT_NAN_MANTISSA: u32 = 0x007f_ffff;
const FLOAT_POSITIVE_INFINITY_MANTISSA: u32 = 0x007f_fffe;
const FLOAT_RESERVED_POSITIVE_MANTISSA: u32 = 0x007f_fffd;
const FLOAT_NRES_MANTISSA: u32 = 0x0080_0000;
const FLOAT_RESERVED_NEGATIVE_MANTISSA: u32 = 0x0080_0001;
const FLOAT_NEGATIVE_INFINITY_MANTISSA: u32 = 0x0080_0002;

const SFLOAT_NAN_MANTISSA: u16 = 0x07ff;
const SFLOAT_POSITIVE_INFINITY_MANTISSA: u16 = 0x07fe;
const SFLOAT_RESERVED_POSITIVE_MANTISSA: u16 = 0x07fd;
const SFLOAT_NRES_MANTISSA: u16 = 0x0800;
const SFLOAT_RESERVED_NEGATIVE_MANTISSA: u16 = 0x0801;
const SFLOAT_NEGATIVE_INFINITY_MANTISSA: u16 = 0x0802;

fn sign_extend_u32_to_i32(value: u32, width: u32) -> i32 {
    let sign = 1u64 << (width - 1);
    let modulus = 1i64 << width;
    if u64::from(value) >= sign {
        (i64::from(value) - modulus) as i32
    } else {
        value as i32
    }
}

fn decode_special_float(mantissa_bits: u32) -> Result<Option<Ieee11073Value>, ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 FLOAT";
    if mantissa_bits == FLOAT_NAN_MANTISSA {
        return Ok(Some(Ieee11073Value::Nan));
    }
    if mantissa_bits == FLOAT_POSITIVE_INFINITY_MANTISSA {
        return Ok(Some(Ieee11073Value::PositiveInfinity));
    }
    if mantissa_bits == FLOAT_NRES_MANTISSA {
        return Ok(Some(Ieee11073Value::Nres));
    }
    if mantissa_bits == FLOAT_NEGATIVE_INFINITY_MANTISSA {
        return Ok(Some(Ieee11073Value::NegativeInfinity));
    }
    if mantissa_bits == FLOAT_RESERVED_POSITIVE_MANTISSA
        || mantissa_bits == FLOAT_RESERVED_NEGATIVE_MANTISSA
    {
        let mut detail = String::from("reserved mantissa 0x");
        append_hex_u32(&mut detail, mantissa_bits);
        return Err(codec_error(ProfileCodecCode::Reserved, CODEC, detail, None));
    }
    Ok(None)
}

fn decode_special_sfloat(mantissa_bits: u16) -> Result<Option<Ieee11073Value>, ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 SFLOAT";
    if mantissa_bits == SFLOAT_NAN_MANTISSA {
        return Ok(Some(Ieee11073Value::Nan));
    }
    if mantissa_bits == SFLOAT_POSITIVE_INFINITY_MANTISSA {
        return Ok(Some(Ieee11073Value::PositiveInfinity));
    }
    if mantissa_bits == SFLOAT_NRES_MANTISSA {
        return Ok(Some(Ieee11073Value::Nres));
    }
    if mantissa_bits == SFLOAT_NEGATIVE_INFINITY_MANTISSA {
        return Ok(Some(Ieee11073Value::NegativeInfinity));
    }
    if mantissa_bits == SFLOAT_RESERVED_POSITIVE_MANTISSA
        || mantissa_bits == SFLOAT_RESERVED_NEGATIVE_MANTISSA
    {
        let mut detail = String::from("reserved mantissa 0x");
        append_hex_u16(&mut detail, mantissa_bits);
        return Err(codec_error(ProfileCodecCode::Reserved, CODEC, detail, None));
    }
    Ok(None)
}

fn append_hex_u32(into: &mut String, mut value: u32) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    if value == 0 {
        into.push('0');
        return;
    }
    let mut digits: [u8; 8] = [0; 8];
    let mut count = 0usize;
    while value > 0 && count < digits.len() {
        digits[count] = HEX[(value & 0x0f) as usize];
        value >>= 4;
        count += 1;
    }
    while count > 0 {
        count -= 1;
        into.push(digits[count] as char);
    }
}

fn append_hex_u16(into: &mut String, value: u16) {
    append_hex_u32(into, u32::from(value));
}

/// Decodes one IEEE-11073 FLOAT at `offset` (little-endian).
pub fn decode_ieee11073_float(
    bytes: &[u8],
    offset: usize,
) -> Result<Ieee11073Value, ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 FLOAT";
    let raw = read_u32_le(bytes, offset, CODEC)?;
    let mantissa_bits = raw & 0x00ff_ffff;
    let exponent = sign_extend_u32_to_i32(raw >> 24, 8) as i8;
    if let Some(special) = decode_special_float(mantissa_bits)? {
        return Ok(special);
    }
    let mantissa = sign_extend_u32_to_i32(mantissa_bits, 24);
    Ok(Ieee11073Value::Finite {
        mantissa,
        exponent,
        value: f64::from(mantissa) * 10f64.powi(i32::from(exponent)),
    })
}

/// Decodes one IEEE-11073 SFLOAT at `offset` (little-endian).
pub fn decode_ieee11073_sfloat(
    bytes: &[u8],
    offset: usize,
) -> Result<Ieee11073Value, ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 SFLOAT";
    let raw = read_u16_le(bytes, offset, CODEC)?;
    let mantissa_bits = raw & 0x0fff;
    let exponent = sign_extend_u32_to_i32(u32::from(raw >> 12), 4) as i8;
    if let Some(special) = decode_special_sfloat(mantissa_bits)? {
        return Ok(special);
    }
    let mantissa = sign_extend_u32_to_i32(u32::from(mantissa_bits), 12);
    Ok(Ieee11073Value::Finite {
        mantissa,
        exponent,
        value: f64::from(mantissa) * 10f64.powi(i32::from(exponent)),
    })
}

/// Decoded Bluetooth SIG Date-Time (7 octets, zero = absent date fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BluetoothDateTime {
    year: Option<u16>,
    month: Option<u8>,
    day: Option<u8>,
    hours: u8,
    minutes: u8,
    seconds: u8,
}

impl BluetoothDateTime {
    /// Borrow the year (`None` = unknown).
    #[must_use]
    pub const fn year(&self) -> Option<u16> {
        self.year
    }

    /// Borrow the month (`None` = unknown).
    #[must_use]
    pub const fn month(&self) -> Option<u8> {
        self.month
    }

    /// Borrow the day (`None` = unknown).
    #[must_use]
    pub const fn day(&self) -> Option<u8> {
        self.day
    }

    /// Borrow the hours (0-23).
    #[must_use]
    pub const fn hours(&self) -> u8 {
        self.hours
    }

    /// Borrow the minutes (0-59).
    #[must_use]
    pub const fn minutes(&self) -> u8 {
        self.minutes
    }

    /// Borrow the seconds (0-59).
    #[must_use]
    pub const fn seconds(&self) -> u8 {
        self.seconds
    }
}

fn assert_integer_in_range(
    value: u16,
    minimum: u16,
    maximum: u16,
    codec: &'static str,
    label: &'static str,
) -> Result<(), ProfileCodecError> {
    if value < minimum || value > maximum {
        let mut detail = String::from(label);
        detail.push_str(" must be an integer from ");
        append_usize(&mut detail, usize::from(minimum));
        detail.push_str(" through ");
        append_usize(&mut detail, usize::from(maximum));
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            codec,
            detail,
            None,
        ));
    }
    Ok(())
}

/// Decodes one Bluetooth SIG Date-Time at `offset`.
pub fn decode_bluetooth_date_time(
    bytes: &[u8],
    offset: usize,
    codec: &'static str,
) -> Result<BluetoothDateTime, ProfileCodecError> {
    let year_value = read_u16_le(bytes, offset, codec)?;
    let month_value = read_u8(bytes, offset + 2, codec)?;
    let day_value = read_u8(bytes, offset + 3, codec)?;
    let hours = read_u8(bytes, offset + 4, codec)?;
    let minutes = read_u8(bytes, offset + 5, codec)?;
    let seconds = read_u8(bytes, offset + 6, codec)?;
    if year_value != 0 && !(1582..=9999).contains(&year_value) {
        let mut detail = String::from("year ");
        append_usize(&mut detail, usize::from(year_value));
        detail.push_str(" is outside the SIG range");
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            codec,
            detail,
            None,
        ));
    }
    if month_value > 12 || day_value > 31 {
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            codec,
            String::from("month or day is outside the SIG range"),
            None,
        ));
    }
    assert_integer_in_range(hours as u16, 0, 23, codec, "hours")?;
    assert_integer_in_range(minutes as u16, 0, 59, codec, "minutes")?;
    assert_integer_in_range(seconds as u16, 0, 59, codec, "seconds")?;
    Ok(BluetoothDateTime {
        year: if year_value == 0 {
            None
        } else {
            Some(year_value)
        },
        month: if month_value == 0 {
            None
        } else {
            Some(month_value)
        },
        day: if day_value == 0 {
            None
        } else {
            Some(day_value)
        },
        hours,
        minutes,
        seconds,
    })
}

pub const HEALTH_THERMOMETER_SERVICE: &str = "00001809-0000-1000-8000-00805f9b34fb";
pub const TEMPERATURE_MEASUREMENT_CHARACTERISTIC: &str = "00002a1c-0000-1000-8000-00805f9b34fb";
pub const INTERMEDIATE_TEMPERATURE_CHARACTERISTIC: &str = "00002a1e-0000-1000-8000-00805f9b34fb";

/// Temperature unit from a Health Thermometer payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemperatureUnit {
    Celsius,
    Fahrenheit,
}

/// Temperature sensor location (`None` on the wire = absent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemperatureType {
    Armpit,
    Body,
    Ear,
    Finger,
    GastroIntestinalTract,
    Mouth,
    Rectum,
    Toe,
    Tympanum,
}

/// Decoded Health Thermometer Temperature Measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TemperatureMeasurement {
    unit: TemperatureUnit,
    temperature: Ieee11073Value,
    timestamp: Option<BluetoothDateTime>,
    sensor_type: Option<TemperatureType>,
}

impl TemperatureMeasurement {
    /// Borrow the unit.
    #[must_use]
    pub const fn unit(&self) -> TemperatureUnit {
        self.unit
    }

    /// Borrow the temperature value.
    #[must_use]
    pub const fn temperature(&self) -> Ieee11073Value {
        self.temperature
    }

    /// Borrow the timestamp (`None` = absent).
    #[must_use]
    pub const fn timestamp(&self) -> Option<BluetoothDateTime> {
        self.timestamp
    }

    /// Borrow the sensor location (`None` = absent).
    #[must_use]
    pub const fn sensor_type(&self) -> Option<TemperatureType> {
        self.sensor_type
    }
}

/// Parses Health Thermometer Temperature Measurement payloads (0x2A1C/0x2A1E).
pub fn parse_temperature_measurement(
    bytes: &[u8],
) -> Result<TemperatureMeasurement, ProfileCodecError> {
    const CODEC: &str = "Health Thermometer Temperature Measurement";
    let flags = read_u8(bytes, 0, CODEC)?;
    assert_no_reserved_flag_bits(flags, 0x07, CODEC)?;
    let timestamp_present = flags & 0x02 != 0;
    let type_present = flags & 0x04 != 0;
    let mut offset = 1usize;
    let temperature = decode_ieee11073_float(bytes, offset)?;
    offset += 4;
    let mut timestamp: Option<BluetoothDateTime> = None;
    if timestamp_present {
        timestamp = Some(decode_bluetooth_date_time(bytes, offset, CODEC)?);
        offset += 7;
    }
    let mut sensor_type: Option<TemperatureType> = None;
    if type_present {
        sensor_type = Some(parse_temperature_type(
            read_u8(bytes, offset, CODEC)?,
            CODEC,
        )?);
        offset += 1;
    }
    if offset != bytes.len() {
        let mut detail = String::from("unexpected ");
        append_usize(&mut detail, bytes.len().saturating_sub(offset));
        detail.push_str(" trailing bytes");
        return Err(codec_error(
            ProfileCodecCode::Malformed,
            CODEC,
            detail,
            Some(offset),
        ));
    }
    Ok(TemperatureMeasurement {
        unit: if flags & 0x01 == 0 {
            TemperatureUnit::Celsius
        } else {
            TemperatureUnit::Fahrenheit
        },
        temperature,
        timestamp,
        sensor_type,
    })
}

fn parse_temperature_type(
    value: u8,
    codec: &'static str,
) -> Result<TemperatureType, ProfileCodecError> {
    match value {
        1 => Ok(TemperatureType::Armpit),
        2 => Ok(TemperatureType::Body),
        3 => Ok(TemperatureType::Ear),
        4 => Ok(TemperatureType::Finger),
        5 => Ok(TemperatureType::GastroIntestinalTract),
        6 => Ok(TemperatureType::Mouth),
        7 => Ok(TemperatureType::Rectum),
        8 => Ok(TemperatureType::Toe),
        9 => Ok(TemperatureType::Tympanum),
        other => {
            let mut detail = String::from("reserved temperature type ");
            append_usize(&mut detail, usize::from(other));
            Err(codec_error(ProfileCodecCode::Reserved, codec, detail, None))
        }
    }
}

pub const BLOOD_PRESSURE_SERVICE: &str = "00001810-0000-1000-8000-00805f9b34fb";
pub const BLOOD_PRESSURE_MEASUREMENT_CHARACTERISTIC: &str = "00002a35-0000-1000-8000-00805f9b34fb";
pub const INTERMEDIATE_CUFF_PRESSURE_CHARACTERISTIC: &str = "00002a36-0000-1000-8000-00805f9b34fb";
pub const BLOOD_PRESSURE_FEATURE_CHARACTERISTIC: &str = "00002a49-0000-1000-8000-00805f9b34fb";

/// Pressure unit from a Blood Pressure payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BloodPressureUnit {
    MillimetresOfMercury,
    Kilopascals,
}

/// Decoded Blood Pressure Measurement (0x2A35, shared with 0x2A36).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BloodPressureMeasurement {
    unit: BloodPressureUnit,
    systolic: Ieee11073Value,
    diastolic: Ieee11073Value,
    mean_arterial_pressure: Ieee11073Value,
    timestamp: Option<BluetoothDateTime>,
    pulse_rate: Option<Ieee11073Value>,
    user_id: Option<u8>,
    user_id_is_unknown: bool,
    measurement_status: Option<u16>,
}

impl BloodPressureMeasurement {
    /// Borrow the pressure unit.
    #[must_use]
    pub const fn unit(&self) -> BloodPressureUnit {
        self.unit
    }

    /// Borrow the systolic value.
    #[must_use]
    pub const fn systolic(&self) -> Ieee11073Value {
        self.systolic
    }

    /// Borrow the diastolic value.
    #[must_use]
    pub const fn diastolic(&self) -> Ieee11073Value {
        self.diastolic
    }

    /// Borrow the mean arterial pressure value.
    #[must_use]
    pub const fn mean_arterial_pressure(&self) -> Ieee11073Value {
        self.mean_arterial_pressure
    }

    /// Borrow the timestamp (`None` = absent).
    #[must_use]
    pub const fn timestamp(&self) -> Option<BluetoothDateTime> {
        self.timestamp
    }

    /// Borrow the pulse rate (`None` = absent).
    #[must_use]
    pub const fn pulse_rate(&self) -> Option<Ieee11073Value> {
        self.pulse_rate
    }

    /// Borrow the user id (`None` = absent).
    #[must_use]
    pub const fn user_id(&self) -> Option<u8> {
        self.user_id
    }

    /// Whether the payload carries the unknown-user sentinel (0xFF).
    #[must_use]
    pub const fn user_id_is_unknown(&self) -> bool {
        self.user_id_is_unknown
    }

    /// Borrow the measurement status (`None` = absent).
    #[must_use]
    pub const fn measurement_status(&self) -> Option<u16> {
        self.measurement_status
    }
}

/// Parses Blood Pressure Measurement and Intermediate Cuff Pressure payloads.
pub fn parse_blood_pressure_measurement(
    bytes: &[u8],
) -> Result<BloodPressureMeasurement, ProfileCodecError> {
    const CODEC: &str = "Blood Pressure Measurement";
    let flags = read_u8(bytes, 0, CODEC)?;
    assert_no_reserved_flag_bits(flags, 0x1f, CODEC)?;
    let timestamp_present = flags & 0x02 != 0;
    let pulse_rate_present = flags & 0x04 != 0;
    let user_id_present = flags & 0x08 != 0;
    let measurement_status_present = flags & 0x10 != 0;
    let mut offset = 1usize;
    let systolic = decode_ieee11073_sfloat(bytes, offset)?;
    offset += 2;
    let diastolic = decode_ieee11073_sfloat(bytes, offset)?;
    offset += 2;
    let mean_arterial_pressure = decode_ieee11073_sfloat(bytes, offset)?;
    offset += 2;
    let mut timestamp: Option<BluetoothDateTime> = None;
    if timestamp_present {
        timestamp = Some(decode_bluetooth_date_time(bytes, offset, CODEC)?);
        offset += 7;
    }
    let mut pulse_rate: Option<Ieee11073Value> = None;
    if pulse_rate_present {
        pulse_rate = Some(decode_ieee11073_sfloat(bytes, offset)?);
        offset += 2;
    }
    let mut user_id: Option<u8> = None;
    if user_id_present {
        user_id = Some(read_u8(bytes, offset, CODEC)?);
        offset += 1;
    }
    let mut measurement_status: Option<u16> = None;
    if measurement_status_present {
        let status = read_u16_le(bytes, offset, CODEC)?;
        if status & 0xffc0 != 0 {
            return Err(codec_error(
                ProfileCodecCode::Reserved,
                CODEC,
                String::from("measurement status has reserved bits set"),
                Some(offset),
            ));
        }
        measurement_status = Some(status);
        offset += 2;
    }
    if offset != bytes.len() {
        let mut detail = String::from("unexpected ");
        append_usize(&mut detail, bytes.len().saturating_sub(offset));
        detail.push_str(" trailing bytes");
        return Err(codec_error(
            ProfileCodecCode::Malformed,
            CODEC,
            detail,
            Some(offset),
        ));
    }
    Ok(BloodPressureMeasurement {
        unit: if flags & 0x01 == 0 {
            BloodPressureUnit::MillimetresOfMercury
        } else {
            BloodPressureUnit::Kilopascals
        },
        systolic,
        diastolic,
        mean_arterial_pressure,
        timestamp,
        pulse_rate,
        user_id,
        user_id_is_unknown: user_id == Some(0xff),
        measurement_status,
    })
}

pub const DEVICE_INFORMATION_SERVICE: &str = "0000180a-0000-1000-8000-00805f9b34fb";
pub const MANUFACTURER_NAME_CHARACTERISTIC: &str = "00002a29-0000-1000-8000-00805f9b34fb";
pub const MODEL_NUMBER_CHARACTERISTIC: &str = "00002a24-0000-1000-8000-00805f9b34fb";
pub const SERIAL_NUMBER_CHARACTERISTIC: &str = "00002a25-0000-1000-8000-00805f9b34fb";
pub const HARDWARE_REVISION_CHARACTERISTIC: &str = "00002a27-0000-1000-8000-00805f9b34fb";
pub const FIRMWARE_REVISION_CHARACTERISTIC: &str = "00002a26-0000-1000-8000-00805f9b34fb";
pub const SOFTWARE_REVISION_CHARACTERISTIC: &str = "00002a28-0000-1000-8000-00805f9b34fb";
pub const SYSTEM_ID_CHARACTERISTIC: &str = "00002a23-0000-1000-8000-00805f9b34fb";
pub const PNP_ID_CHARACTERISTIC: &str = "00002a50-0000-1000-8000-00805f9b34fb";

/// Device Information string characteristic selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceInformationStringField {
    ManufacturerName,
    ModelNumber,
    SerialNumber,
    HardwareRevision,
    FirmwareRevision,
    SoftwareRevision,
}

impl DeviceInformationStringField {
    /// Borrow the characteristic UUID for this string field.
    #[must_use]
    pub const fn characteristic_uuid(self) -> &'static str {
        match self {
            Self::ManufacturerName => MANUFACTURER_NAME_CHARACTERISTIC,
            Self::ModelNumber => MODEL_NUMBER_CHARACTERISTIC,
            Self::SerialNumber => SERIAL_NUMBER_CHARACTERISTIC,
            Self::HardwareRevision => HARDWARE_REVISION_CHARACTERISTIC,
            Self::FirmwareRevision => FIRMWARE_REVISION_CHARACTERISTIC,
            Self::SoftwareRevision => SOFTWARE_REVISION_CHARACTERISTIC,
        }
    }
}

/// Decodes the mandatory UTF-8 representation of DIS string characteristics.
pub fn decode_device_information_string(bytes: &[u8]) -> Result<String, ProfileCodecError> {
    const CODEC: &str = "Device Information String";
    match core::str::from_utf8(bytes) {
        Ok(value) => Ok(String::from(value)),
        Err(_) => Err(codec_error(
            ProfileCodecCode::Malformed,
            CODEC,
            String::from("UTF-8 decoder rejected the value"),
            None,
        )),
    }
}

/// Decoded System ID (0x2A23): uint40 manufacturer id + uint24 OUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemId {
    manufacturer_identifier: u64,
    organizationally_unique_identifier: u32,
}

impl SystemId {
    /// Borrow the uint40 manufacturer identifier.
    #[must_use]
    pub const fn manufacturer_identifier(&self) -> u64 {
        self.manufacturer_identifier
    }

    /// Borrow the uint24 organizationally unique identifier.
    #[must_use]
    pub const fn organizationally_unique_identifier(&self) -> u32 {
        self.organizationally_unique_identifier
    }
}

/// Parses System ID (0x2A23), both fields little-endian.
pub fn parse_system_id(bytes: &[u8]) -> Result<SystemId, ProfileCodecError> {
    const CODEC: &str = "System ID";
    require_exact_length(bytes, 8, CODEC)?;
    let mut manufacturer_identifier: u64 = 0;
    for offset in 0..5 {
        manufacturer_identifier |= u64::from(read_u8(bytes, offset, CODEC)?) << (offset * 8);
    }
    let organizationally_unique_identifier = u32::from(read_u8(bytes, 5, CODEC)?)
        | (u32::from(read_u8(bytes, 6, CODEC)?) << 8)
        | (u32::from(read_u8(bytes, 7, CODEC)?) << 16);
    Ok(SystemId {
        manufacturer_identifier,
        organizationally_unique_identifier,
    })
}

/// Vendor ID source from a PnP ID payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VendorIdSource {
    BluetoothSig,
    UsbImplementersForum,
}

/// Decoded PnP ID (0x2A50).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PnpId {
    vendor_id_source: VendorIdSource,
    vendor_id: u16,
    product_id: u16,
    product_version: u16,
}

impl PnpId {
    /// Borrow the vendor ID source.
    #[must_use]
    pub const fn vendor_id_source(&self) -> VendorIdSource {
        self.vendor_id_source
    }

    /// Borrow the vendor id.
    #[must_use]
    pub const fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    /// Borrow the product id.
    #[must_use]
    pub const fn product_id(&self) -> u16 {
        self.product_id
    }

    /// Borrow the product version.
    #[must_use]
    pub const fn product_version(&self) -> u16 {
        self.product_version
    }
}

/// Parses PnP ID (0x2A50), rejecting reserved Vendor ID Source values.
pub fn parse_pnp_id(bytes: &[u8]) -> Result<PnpId, ProfileCodecError> {
    const CODEC: &str = "PnP ID";
    require_exact_length(bytes, 7, CODEC)?;
    let vendor_id_source = read_u8(bytes, 0, CODEC)?;
    if vendor_id_source != 1 && vendor_id_source != 2 {
        let mut detail = String::from("reserved vendor ID source ");
        append_usize(&mut detail, usize::from(vendor_id_source));
        return Err(codec_error(ProfileCodecCode::Reserved, CODEC, detail, None));
    }
    Ok(PnpId {
        vendor_id_source: if vendor_id_source == 1 {
            VendorIdSource::BluetoothSig
        } else {
            VendorIdSource::UsbImplementersForum
        },
        vendor_id: read_u16_le(bytes, 1, CODEC)?,
        product_id: read_u16_le(bytes, 3, CODEC)?,
        product_version: read_u16_le(bytes, 5, CODEC)?,
    })
}

fn assert_i32_in_range(
    value: i32,
    minimum: i32,
    maximum: i32,
    codec: &'static str,
    label: &'static str,
) -> Result<(), ProfileCodecError> {
    if value < minimum || value > maximum {
        let mut detail = String::from(label);
        detail.push_str(" must be an integer from ");
        append_i32(&mut detail, minimum);
        detail.push_str(" through ");
        append_i32(&mut detail, maximum);
        return Err(codec_error(
            ProfileCodecCode::InvalidValue,
            codec,
            detail,
            None,
        ));
    }
    Ok(())
}

fn append_i32(into: &mut String, value: i32) {
    if value < 0 {
        into.push('-');
        append_usize(into, (value as i64).unsigned_abs() as usize);
    } else {
        append_usize(into, value as usize);
    }
}

fn special_float_mantissa(kind: &Ieee11073Value) -> u32 {
    match kind {
        Ieee11073Value::Nan => FLOAT_NAN_MANTISSA,
        Ieee11073Value::Nres => FLOAT_NRES_MANTISSA,
        Ieee11073Value::PositiveInfinity => FLOAT_POSITIVE_INFINITY_MANTISSA,
        Ieee11073Value::NegativeInfinity => FLOAT_NEGATIVE_INFINITY_MANTISSA,
        Ieee11073Value::Finite { .. } => 0,
    }
}

fn special_sfloat_mantissa(kind: &Ieee11073Value) -> u16 {
    match kind {
        Ieee11073Value::Nan => SFLOAT_NAN_MANTISSA,
        Ieee11073Value::Nres => SFLOAT_NRES_MANTISSA,
        Ieee11073Value::PositiveInfinity => SFLOAT_POSITIVE_INFINITY_MANTISSA,
        Ieee11073Value::NegativeInfinity => SFLOAT_NEGATIVE_INFINITY_MANTISSA,
        Ieee11073Value::Finite { .. } => 0,
    }
}

/// Encodes one IEEE-11073 FLOAT (little-endian), mirroring the oracle's
/// range validation exactly.
pub fn encode_ieee11073_float(value: &Ieee11073Value) -> Result<[u8; 4], ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 FLOAT";
    let (mantissa_bits, exponent_bits) = match *value {
        Ieee11073Value::Finite {
            mantissa, exponent, ..
        } => {
            assert_i32_in_range(mantissa, -0x80_0000, 0x7f_ffff, CODEC, "mantissa")?;
            assert_i32_in_range(i32::from(exponent), -128, 127, CODEC, "exponent")?;
            let bits = (mantissa as u32) & 0x00ff_ffff;
            // A finite value whose bit pattern hits a reserved mantissa is
            // rejected, exactly like the oracle's encode path.
            let _ = decode_special_float(bits)?;
            (bits, (exponent as u32) & 0xff)
        }
        special => (special_float_mantissa(&special), 0),
    };
    let raw = mantissa_bits | (exponent_bits << 24);
    Ok(raw.to_le_bytes())
}

/// Encodes one IEEE-11073 SFLOAT (little-endian), mirroring the oracle.
pub fn encode_ieee11073_sfloat(value: &Ieee11073Value) -> Result<[u8; 2], ProfileCodecError> {
    const CODEC: &str = "IEEE-11073 SFLOAT";
    let (mantissa_bits, exponent_bits) = match *value {
        Ieee11073Value::Finite {
            mantissa, exponent, ..
        } => {
            assert_i32_in_range(mantissa, -2048, 2047, CODEC, "mantissa")?;
            assert_i32_in_range(i32::from(exponent), -8, 7, CODEC, "exponent")?;
            let bits = (mantissa as u16) & 0x0fff;
            let _ = decode_special_sfloat(bits)?;
            (u32::from(bits), (exponent as u32) & 0x0f)
        }
        special => (u32::from(special_sfloat_mantissa(&special)), 0),
    };
    let raw = (mantissa_bits | (exponent_bits << 12)) as u16;
    Ok(raw.to_le_bytes())
}

/// Sensor-contact state from a Heart Rate Measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartRateContact {
    Unsupported,
    NotDetected,
    Detected,
}

/// Decoded Heart Rate Measurement (0x2A37). Units/absence mirror the
/// oracle exactly: RR intervals in seconds, energy expended kept as the
/// raw UINT16 value (or absent).
#[derive(Debug, Clone, PartialEq)]
pub struct HeartRateMeasurement {
    beats_per_minute: u16,
    contact: HeartRateContact,
    energy_expended_kilojoules: Option<u16>,
    rr_intervals_seconds: Vec<f64>,
}

impl HeartRateMeasurement {
    /// Borrow the beats-per-minute value.
    #[must_use]
    pub const fn beats_per_minute(&self) -> u16 {
        self.beats_per_minute
    }

    /// Borrow the sensor-contact state.
    #[must_use]
    pub const fn contact(&self) -> HeartRateContact {
        self.contact
    }

    /// Borrow the raw energy-expended value (`None` = absent).
    #[must_use]
    pub const fn energy_expended_kilojoules(&self) -> Option<u16> {
        self.energy_expended_kilojoules
    }

    /// Borrow the RR intervals in seconds (wire unit: 1/1024 second).
    #[must_use]
    pub fn rr_intervals_seconds(&self) -> &[f64] {
        &self.rr_intervals_seconds
    }
}

/// Parses the Bluetooth SIG Heart Rate Measurement characteristic (0x2A37).
pub fn parse_heart_rate_measurement(
    bytes: &[u8],
) -> Result<HeartRateMeasurement, ProfileCodecError> {
    const CODEC: &str = "Heart Rate Measurement";
    let flags = read_u8(bytes, 0, CODEC)?;
    assert_no_reserved_flag_bits(flags, 0x1f, CODEC)?;
    let value_is_uint16 = flags & 0x01 != 0;
    let contact_status = flags & 0x02 != 0;
    let contact_supported = flags & 0x04 != 0;
    if contact_status && !contact_supported {
        return Err(codec_error(
            ProfileCodecCode::Reserved,
            CODEC,
            String::from("sensor contact status is set without sensor-contact support"),
            None,
        ));
    }
    let energy_present = flags & 0x08 != 0;
    let rr_present = flags & 0x10 != 0;
    let mut offset = 1usize;
    let beats_per_minute = if value_is_uint16 {
        let value = read_u16_le(bytes, offset, CODEC)?;
        offset += 2;
        value
    } else {
        let value = read_u8(bytes, offset, CODEC)?;
        offset += 1;
        u16::from(value)
    };
    let mut energy_expended_kilojoules: Option<u16> = None;
    if energy_present {
        energy_expended_kilojoules = Some(read_u16_le(bytes, offset, CODEC)?);
        offset += 2;
    }
    if !rr_present && offset != bytes.len() {
        let mut detail = String::from("unexpected ");
        append_usize(&mut detail, bytes.len().saturating_sub(offset));
        detail.push_str(" trailing bytes");
        return Err(codec_error(
            ProfileCodecCode::Malformed,
            CODEC,
            detail,
            Some(offset),
        ));
    }
    let remaining = bytes.len().saturating_sub(offset);
    if rr_present && !remaining.is_multiple_of(2) {
        return Err(codec_error(
            ProfileCodecCode::Truncated,
            CODEC,
            String::from("RR-interval list has an incomplete UINT16 value"),
            Some(offset),
        ));
    }
    let mut rr_intervals_seconds: Vec<f64> = Vec::new();
    while offset < bytes.len() {
        let raw = read_u16_le(bytes, offset, CODEC)?;
        rr_intervals_seconds.push(f64::from(raw) / 1024.0);
        offset += 2;
    }
    Ok(HeartRateMeasurement {
        beats_per_minute,
        contact: if contact_supported {
            if contact_status {
                HeartRateContact::Detected
            } else {
                HeartRateContact::NotDetected
            }
        } else {
            HeartRateContact::Unsupported
        },
        energy_expended_kilojoules,
        rr_intervals_seconds,
    })
}

/// Parses Body Sensor Location (0x2A29-class 0x2A38): exactly one byte, 0-6.
pub fn parse_body_sensor_location(bytes: &[u8]) -> Result<u8, ProfileCodecError> {
    const CODEC: &str = "Body Sensor Location";
    let location = read_u8(bytes, 0, CODEC)?;
    if bytes.len() != 1 {
        let mut detail = String::from("requires exactly one byte; received ");
        append_usize(&mut detail, bytes.len());
        return Err(codec_error(
            ProfileCodecCode::Malformed,
            CODEC,
            detail,
            None,
        ));
    }
    if location > 6 {
        let mut detail = String::from("reserved body sensor location ");
        append_usize(&mut detail, usize::from(location));
        return Err(codec_error(ProfileCodecCode::Reserved, CODEC, detail, None));
    }
    Ok(location)
}

/// Exact control-point value for the HRS Reset Energy Expended command.
#[must_use]
pub const fn encode_reset_energy_expended() -> [u8; 1] {
    [0x01]
}

/// Builds a characteristic [`PathSelector`] for one profile attribute.
#[must_use]
pub fn profile_selector(
    service_uuid: &str,
    characteristic_uuid: &str,
    service_occurrence: Option<u64>,
    characteristic_occurrence: Option<u64>,
) -> PathSelector {
    PathSelector {
        service_uuid: String::from(service_uuid),
        service_occurrence,
        characteristic_uuid: Some(String::from(characteristic_uuid)),
        characteristic_occurrence,
        descriptor_uuid: None,
        descriptor_occurrence: None,
    }
}

/// Resolves one profile characteristic path through the real central
/// (GATT-04 duplicate guard included). Lifecycle stays with the caller.
pub fn resolve_profile_path(
    central: &Central,
    peer_key: &str,
    service_uuid: &str,
    service_occurrence: Option<u64>,
    characteristic_uuid: &str,
    characteristic_occurrence: Option<u64>,
) -> Result<usize, CoreError> {
    let selector = PathSelector {
        service_uuid: canonical_uuid(service_uuid)?,
        service_occurrence,
        characteristic_uuid: Some(canonical_uuid(characteristic_uuid)?),
        characteristic_occurrence,
        descriptor_uuid: None,
        descriptor_occurrence: None,
    };
    central.resolve_path(peer_key, &selector)
}

/// Maps a contract failure into the frozen error space (thin helper so
/// profile call sites never invent codes).
#[allow(dead_code)]
fn contract_err(code: BleErrorCode, domain: BleErrorDomain, operation: &'static str) -> CoreError {
    CoreError::new(code, domain, operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::{Central, CentralConfig, GATT_PROP_NOTIFY, GATT_PROP_READ};
    use crate::check;
    use crate::contracts::{
        AdapterGeneration, AdapterId, AttachmentId, AttachmentTuple, BackendGeneration,
        BackendInstanceId, ContenderKind, Generation,
    };
    use crate::ownership::EffectBatch;

    fn ok<T>(result: Result<T, ProfileCodecError>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => {
                check(false, what);
                // `check` always panics on failure; retain a value without a
                // halting macro on this path.
                let _ = error;
                loop {
                    core::hint::spin_loop();
                }
            }
        }
    }

    fn expect_codec<T>(result: Result<T, ProfileCodecError>, code: ProfileCodecCode, what: &str) {
        match result {
            Err(error) => {
                check(error.code() == code, what);
            }
            Ok(_) => {
                check(false, what);
            }
        }
    }

    #[test]
    fn battery_level_round_trip_and_rejections() {
        check(
            ok(parse_battery_level(&[50]), "battery 50 parses") == 50,
            "battery value kept",
        );
        check(
            ok(parse_battery_level(&[0]), "battery 0 parses") == 0,
            "battery zero kept",
        );
        check(
            ok(parse_battery_level(&[100]), "battery 100 parses") == 100,
            "battery full kept",
        );
        check(
            ok(encode_battery_level(50), "battery 50 encodes") == [50],
            "battery encoding kept",
        );
        expect_codec(
            parse_battery_level(&[101]),
            ProfileCodecCode::InvalidValue,
            "battery 101 rejected",
        );
        expect_codec(
            parse_battery_level(&[50, 1]),
            ProfileCodecCode::Malformed,
            "battery trailing byte rejected",
        );
        expect_codec(
            parse_battery_level(&[]),
            ProfileCodecCode::Malformed,
            "battery empty input rejected",
        );
        expect_codec(
            encode_battery_level(101),
            ProfileCodecCode::InvalidValue,
            "battery encode 101 rejected",
        );
    }

    #[test]
    fn heart_rate_measurement_vectors() {
        let simple = ok(parse_heart_rate_measurement(&[0x04, 81]), "hr u8 parses");
        check(simple.beats_per_minute() == 81, "hr bpm kept");
        check(
            simple.contact() == HeartRateContact::NotDetected,
            "hr contact kept",
        );
        check(
            simple.energy_expended_kilojoules().is_none(),
            "hr energy absent",
        );
        check(simple.rr_intervals_seconds().is_empty(), "hr rr absent");

        let detected = ok(
            parse_heart_rate_measurement(&[0x06, 72]),
            "hr contact parses",
        );
        check(
            detected.contact() == HeartRateContact::Detected,
            "hr contact detected",
        );

        let wide = ok(
            parse_heart_rate_measurement(&[0x01, 0x2c, 0x01]),
            "hr u16 parses",
        );
        check(wide.beats_per_minute() == 300, "hr u16 bpm kept");

        let full = ok(
            parse_heart_rate_measurement(&[0x1e, 0x48, 0x0a, 0x00, 0x00, 0x04, 0x00, 0x08]),
            "hr full parses",
        );
        check(full.beats_per_minute() == 72, "hr full bpm kept");
        check(
            full.energy_expended_kilojoules() == Some(10),
            "hr energy value kept",
        );
        check(full.rr_intervals_seconds().len() == 2, "hr rr count kept");
        check(
            (full.rr_intervals_seconds()[0] - 1.0).abs() < 1e-12,
            "hr rr unit seconds",
        );

        expect_codec(
            parse_heart_rate_measurement(&[0x20, 72]),
            ProfileCodecCode::Reserved,
            "hr reserved flags rejected",
        );
        expect_codec(
            parse_heart_rate_measurement(&[0x02, 72]),
            ProfileCodecCode::Reserved,
            "hr contact without support rejected",
        );
        expect_codec(
            parse_heart_rate_measurement(&[0x10, 72, 0]),
            ProfileCodecCode::Truncated,
            "hr partial rr rejected",
        );
        expect_codec(
            parse_heart_rate_measurement(&[0x00, 72, 0x01]),
            ProfileCodecCode::Malformed,
            "hr trailing bytes rejected",
        );
        check(
            encode_reset_energy_expended() == [0x01],
            "hr reset command exact",
        );
        check(
            ok(parse_body_sensor_location(&[3]), "sensor location parses") == 3,
            "sensor location kept",
        );
        expect_codec(
            parse_body_sensor_location(&[7]),
            ProfileCodecCode::Reserved,
            "sensor location reserved rejected",
        );
    }

    #[test]
    fn ieee11073_and_thermometer_vectors() {
        match ok(decode_ieee11073_sfloat(&[0x6e, 0xf1], 0), "sfloat decodes") {
            Ieee11073Value::Finite {
                mantissa,
                exponent,
                value,
            } => {
                check(mantissa == 366, "sfloat mantissa kept");
                check(exponent == -1, "sfloat exponent kept");
                check((value - 36.6).abs() < 1e-9, "sfloat value kept");
            }
            _ => {
                check(false, "sfloat finite expected");
            }
        }
        expect_codec(
            decode_ieee11073_sfloat(&[0xfd, 0x07], 0),
            ProfileCodecCode::Reserved,
            "sfloat reserved rejected",
        );
        let finite = Ieee11073Value::Finite {
            mantissa: 366,
            exponent: -1,
            value: 36.6,
        };
        check(
            ok(encode_ieee11073_sfloat(&finite), "sfloat encodes") == [0x6e, 0xf1],
            "sfloat encoding exact",
        );
        check(
            ok(
                decode_ieee11073_sfloat(
                    &ok(encode_ieee11073_sfloat(&finite), "sfloat re-encodes"),
                    0,
                ),
                "sfloat round trip",
            ) == finite,
            "sfloat round trip kept",
        );
        check(
            ok(encode_ieee11073_float(&finite), "float encodes") == [0x6e, 0x01, 0x00, 0xff],
            "float encoding exact",
        );
        check(
            ok(
                encode_ieee11073_sfloat(&Ieee11073Value::Nan),
                "sfloat nan encodes",
            ) == [0xff, 0x07],
            "sfloat nan exact",
        );
        expect_codec(
            encode_ieee11073_sfloat(&Ieee11073Value::Finite {
                mantissa: 2048,
                exponent: 0,
                value: 2048.0,
            }),
            ProfileCodecCode::InvalidValue,
            "sfloat mantissa range rejected",
        );
        expect_codec(
            encode_ieee11073_sfloat(&Ieee11073Value::Finite {
                mantissa: 1,
                exponent: 8,
                value: 10_000_000.0,
            }),
            ProfileCodecCode::InvalidValue,
            "sfloat exponent range rejected",
        );
        let measurement = ok(
            parse_temperature_measurement(&[0x04, 0x6e, 0x01, 0x00, 0xff, 0x02]),
            "temperature parses",
        );
        check(
            measurement.unit() == TemperatureUnit::Celsius,
            "temperature unit kept",
        );
        check(
            measurement.sensor_type() == Some(TemperatureType::Body),
            "temperature type kept",
        );
        check(
            measurement.timestamp().is_none(),
            "temperature timestamp absent",
        );
        match measurement.temperature() {
            Ieee11073Value::Finite {
                mantissa, value, ..
            } => {
                check(mantissa == 366, "temperature mantissa kept");
                check((value - 36.6).abs() < 1e-6, "temperature value kept");
            }
            _ => {
                check(false, "temperature finite expected");
            }
        }
    }

    #[test]
    fn blood_pressure_and_device_information_vectors() {
        let measurement = ok(
            parse_blood_pressure_measurement(&[0x00, 0x78, 0x00, 0x50, 0x00, 0x5d, 0x00]),
            "bp parses",
        );
        check(
            measurement.unit() == BloodPressureUnit::MillimetresOfMercury,
            "bp unit kept",
        );
        match measurement.systolic() {
            Ieee11073Value::Finite {
                mantissa, value, ..
            } => {
                check(mantissa == 120, "bp systolic mantissa kept");
                check(value == 120.0, "bp systolic value kept");
            }
            _ => {
                check(false, "bp systolic finite expected");
            }
        }
        check(measurement.timestamp().is_none(), "bp timestamp absent");
        check(measurement.pulse_rate().is_none(), "bp pulse absent");
        check(measurement.user_id().is_none(), "bp user absent");
        check(!measurement.user_id_is_unknown(), "bp unknown flag clear");
        check(
            measurement.measurement_status().is_none(),
            "bp status absent",
        );
        expect_codec(
            parse_blood_pressure_measurement(&[0x20, 0, 0, 0, 0, 0, 0]),
            ProfileCodecCode::Reserved,
            "bp reserved flags rejected",
        );

        check(
            ok(
                decode_device_information_string(b"ACME"),
                "dis string decodes",
            ) == "ACME",
            "dis string kept",
        );
        expect_codec(
            decode_device_information_string(&[0xff, 0xfe]),
            ProfileCodecCode::Malformed,
            "dis invalid utf-8 rejected",
        );
        let system_id = ok(
            parse_system_id(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]),
            "system id parses",
        );
        check(
            system_id.manufacturer_identifier() == 0x0005_0403_0201,
            "system id manufacturer kept",
        );
        check(
            system_id.organizationally_unique_identifier() == 0x0008_0706,
            "system id oui kept",
        );
        let pnp = ok(
            parse_pnp_id(&[0x01, 0x34, 0x12, 0x78, 0x56, 0x01, 0x00]),
            "pnp parses",
        );
        check(
            pnp.vendor_id_source() == VendorIdSource::BluetoothSig,
            "pnp source kept",
        );
        check(pnp.vendor_id() == 0x1234, "pnp vendor kept");
        expect_codec(
            parse_pnp_id(&[0x03, 0, 0, 0, 0, 0, 0]),
            ProfileCodecCode::Reserved,
            "pnp reserved source rejected",
        );
    }

    fn live_central_with_profiles() -> Result<(Central, String), CoreError> {
        let attachment = AttachmentTuple::new(
            AttachmentId::new("attach-01")?,
            BackendInstanceId::new("backend-01")?,
            BackendGeneration::new("bg-3")?,
            AdapterId::new("adapter-01")?,
            AdapterGeneration::new("ag-2")?,
        );
        let mut central = Central::new(
            attachment,
            Generation::new("kernel-gen-1")?,
            CentralConfig::default(),
        )?;
        let mut out = EffectBatch::new(64);
        let peer = central.resolve_peer("public-address", "AA:BB:CC:DD:EE:01")?;
        let op = central.connect(&peer, "client-1", 5000, 1000, &mut out)?;
        central.dispatch_op(&op, &mut out)?;
        central.settle_op(&op, ContenderKind::Success, true, 0, 1001, &mut out)?;
        central.note_link_established(&peer)?;
        central.begin_discovery(&peer)?;
        central.complete_discovery(&peer)?;
        central.register_path(
            &peer,
            "180D",
            0,
            Some("2A37"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        central.register_path(
            &peer,
            "180F",
            0,
            Some("2A19"),
            Some(0),
            None,
            None,
            GATT_PROP_READ | GATT_PROP_NOTIFY,
            "lease-1",
        )?;
        Ok((central, peer))
    }

    fn expect_core_code<T>(result: Result<T, CoreError>, code: BleErrorCode, what: &str) {
        match result {
            Err(error) => {
                check(error.code() == code, what);
            }
            Ok(_) => {
                check(false, what);
            }
        }
    }

    #[test]
    fn profile_paths_resolve_through_real_central() -> Result<(), CoreError> {
        let (central, peer) = live_central_with_profiles()?;
        let battery = resolve_profile_path(
            &central,
            &peer,
            BATTERY_SERVICE,
            Some(0),
            BATTERY_LEVEL_CHARACTERISTIC,
            Some(0),
        )?;
        let stored = central.stored_path(battery);
        check(stored.is_some(), "battery path resolves");
        check(
            stored.map(|path| {
                path.characteristic_uuid() == Some(HEART_RATE_MEASUREMENT_CHARACTERISTIC)
            }) == Some(false),
            "battery path is not hr",
        );
        let heart = resolve_profile_path(&central, &peer, "180D", Some(0), "2A37", Some(0))?;
        check(
            central
                .stored_path(heart)
                .map(|path| path.characteristic_uuid())
                == Some(Some(HEART_RATE_MEASUREMENT_CHARACTERISTIC)),
            "hr path resolves via short uuid",
        );
        Ok(())
    }

    #[test]
    fn profile_paths_reject_duplicates_without_occurrence() -> Result<(), CoreError> {
        let (mut central, peer) = live_central_with_profiles()?;
        central.register_path(
            &peer,
            "180F",
            1,
            Some("2A19"),
            Some(0),
            None,
            None,
            GATT_PROP_READ,
            "lease-1",
        )?;
        expect_core_code(
            resolve_profile_path(
                &central,
                &peer,
                BATTERY_SERVICE,
                None,
                BATTERY_LEVEL_CHARACTERISTIC,
                None,
            ),
            BleErrorCode::GattAmbiguousPath,
            "duplicated battery rejects uuid-only",
        );
        let first = resolve_profile_path(
            &central,
            &peer,
            BATTERY_SERVICE,
            Some(0),
            BATTERY_LEVEL_CHARACTERISTIC,
            Some(0),
        )?;
        let second = resolve_profile_path(
            &central,
            &peer,
            BATTERY_SERVICE,
            Some(1),
            BATTERY_LEVEL_CHARACTERISTIC,
            Some(0),
        )?;
        check(first != second, "occurrences resolve to distinct paths");
        check(
            central.stored_path(second).map(|path| path.service_uuid()) == Some(BATTERY_SERVICE),
            "explicit occurrence resolves",
        );
        Ok(())
    }
}
