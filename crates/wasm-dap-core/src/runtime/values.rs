//! Conversions between Wasmtime values and text.

use serde_json::Value;
use wasmtime::{Val, ValType};

use crate::error::{Error, Result};

pub fn val_type_name(val: &Val) -> &'static str {
    match val {
        Val::I32(_) => "i32",
        Val::I64(_) => "i64",
        Val::F32(_) => "f32",
        Val::F64(_) => "f64",
        Val::V128(_) => "v128",
        Val::FuncRef(_) => "funcref",
        Val::ExternRef(_) => "externref",
        Val::AnyRef(_) => "anyref",
        Val::ExnRef(_) => "exnref",
        Val::ContRef(_) => "contref",
    }
}

pub fn format_val(val: &Val, hex: bool) -> String {
    match val {
        Val::I32(v) if hex => format!("{:#010x}", *v as u32),
        Val::I32(v) => v.to_string(),
        Val::I64(v) if hex => format!("{:#018x}", *v as u64),
        Val::I64(v) => v.to_string(),
        Val::F32(bits) => format!("{:?}", f32::from_bits(*bits)),
        Val::F64(bits) => format!("{:?}", f64::from_bits(*bits)),
        Val::V128(v) => format!("0x{:032x}", v.as_u128()),
        Val::FuncRef(f) => if f.is_some() { "funcref" } else { "null" }.to_string(),
        Val::ExternRef(r) => if r.is_some() { "externref" } else { "null" }.to_string(),
        Val::AnyRef(r) => if r.is_some() { "anyref" } else { "null" }.to_string(),
        Val::ExnRef(r) => if r.is_some() { "exnref" } else { "null" }.to_string(),
        Val::ContRef(r) => if r.is_some() { "contref" } else { "null" }.to_string(),
    }
}

/// Little-endian in-memory representation of a numeric value.
pub fn val_to_bytes(val: &Val) -> Option<Vec<u8>> {
    match val {
        Val::I32(v) => Some(v.to_le_bytes().to_vec()),
        Val::I64(v) => Some(v.to_le_bytes().to_vec()),
        Val::F32(bits) => Some(bits.to_le_bytes().to_vec()),
        Val::F64(bits) => Some(bits.to_le_bytes().to_vec()),
        Val::V128(v) => Some(v.as_u128().to_le_bytes().to_vec()),
        _ => None,
    }
}

/// Numeric value of an integer-typed value, sign-extended.
pub fn val_as_i64(val: &Val) -> Option<i64> {
    match val {
        Val::I32(v) => Some(i64::from(*v)),
        Val::I64(v) => Some(*v),
        _ => None,
    }
}

/// Parses an integer literal with optional sign and `0x`/`0o`/`0b` prefix.
pub fn parse_int(text: &str) -> Option<i128> {
    let text = text.trim().replace('_', "");
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, text.trim_start_matches('+').to_string()),
    };
    let lower = digits.to_ascii_lowercase();
    let value = if let Some(hex) = lower.strip_prefix("0x") {
        i128::from_str_radix(hex, 16).ok()?
    } else if let Some(oct) = lower.strip_prefix("0o") {
        i128::from_str_radix(oct, 8).ok()?
    } else if let Some(bin) = lower.strip_prefix("0b") {
        i128::from_str_radix(bin, 2).ok()?
    } else {
        lower.parse::<i128>().ok()?
    };
    Some(if negative { -value } else { value })
}

pub fn parse_float(text: &str) -> Option<f64> {
    let lower = text.trim().to_ascii_lowercase();
    match lower.as_str() {
        "nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => lower.parse::<f64>().ok(),
    }
}

/// Parses text into a value of the given type.
pub fn parse_val_str(text: &str, ty: &ValType) -> Result<Val> {
    let invalid = || Error::Config(format!("`{text}` is not a valid {ty} value"));
    match ty {
        ValType::I32 => {
            let v = parse_int(text).ok_or_else(invalid)?;
            if v < i128::from(i32::MIN) || v > i128::from(u32::MAX) {
                return Err(invalid());
            }
            Ok(Val::I32(v as i64 as i32))
        }
        ValType::I64 => {
            let v = parse_int(text).ok_or_else(invalid)?;
            if v < i128::from(i64::MIN) || v > i128::from(u64::MAX) {
                return Err(invalid());
            }
            Ok(Val::I64(v as i64))
        }
        ValType::F32 => Ok(Val::F32(
            (parse_float(text).ok_or_else(invalid)? as f32).to_bits(),
        )),
        ValType::F64 => Ok(Val::F64(parse_float(text).ok_or_else(invalid)?.to_bits())),
        ValType::V128 => {
            let v = parse_int(text).ok_or_else(invalid)?;
            Ok(Val::V128((v as u128).into()))
        }
        ValType::Ref(_) => Err(Error::Unsupported(format!(
            "reference-typed argument `{text}` cannot be created from text"
        ))),
    }
}

/// Parses a JSON launch argument (number, boolean or string) into a value.
pub fn parse_val(input: &Value, ty: &ValType) -> Result<Val> {
    match input {
        Value::String(s) => parse_val_str(s, ty),
        Value::Bool(b) => parse_val_str(if *b { "1" } else { "0" }, ty),
        Value::Number(n) => parse_val_str(&n.to_string(), ty),
        other => Err(Error::Config(format!(
            "unsupported argument {other}; use a number or a string"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integers_in_several_bases() {
        assert_eq!(parse_int("42"), Some(42));
        assert_eq!(parse_int("-0x10"), Some(-16));
        assert_eq!(parse_int("0b101"), Some(5));
        assert_eq!(parse_int("1_000"), Some(1000));
        assert_eq!(parse_int("abc"), None);
    }

    #[test]
    fn parses_typed_values() {
        assert!(matches!(
            parse_val_str("-1", &ValType::I32),
            Ok(Val::I32(-1))
        ));
        assert!(matches!(
            parse_val_str("4294967295", &ValType::I32),
            Ok(Val::I32(-1))
        ));
        assert!(parse_val_str("4294967296", &ValType::I32).is_err());
        assert!(matches!(
            parse_val(&serde_json::json!(7), &ValType::I64),
            Ok(Val::I64(7))
        ));
        match parse_val(&serde_json::json!("2.5"), &ValType::F64).unwrap() {
            Val::F64(bits) => assert_eq!(f64::from_bits(bits), 2.5),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn formats_values() {
        assert_eq!(format_val(&Val::I32(-5), false), "-5");
        assert_eq!(format_val(&Val::I32(255), true), "0x000000ff");
        assert_eq!(format_val(&Val::F64(1.0f64.to_bits()), false), "1.0");
    }
}
