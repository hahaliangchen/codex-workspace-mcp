//! ECMAScript value conversions over VM values: ToString, ToNumber,
//! ToPrimitive and loose equality. Each follows the specification algorithm
//! for the value kinds this runtime has; nothing is approximated through JSON.
use super::heap::{Heap, HeapId, HeapNode, VmValue};
use anyhow::Result;
use std::collections::BTreeSet;

/// WhiteSpace and LineTerminator code points, as trimmed by String.prototype.trim
/// and skipped by StringToNumber.
pub(super) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Number::toString(10): the shortest round-trip digits, laid out with the
/// specification's thresholds for plain and exponential notation.
pub(super) fn number_to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x == 0.0 {
        return "0".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x < 0.0 {
        return format!("-{}", number_to_string(-x));
    }
    // Rust prints the shortest digits that round-trip, as `d.ddde±x`.
    let scientific = format!("{x:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("scientific formatting always has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i64;
    let n = exponent.parse::<i64>().expect("exponent is an integer") + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let sign = if n - 1 >= 0 { "+" } else { "-" };
        let power = (n - 1).abs();
        if k == 1 {
            format!("{digits}e{sign}{power}")
        } else {
            format!("{}.{}e{sign}{power}", &digits[..1], &digits[1..])
        }
    }
}

/// StringToNumber: whitespace-trimmed decimal, Infinity, or an unsigned
/// 0x/0o/0b integer. Anything else, including Rust-only spellings such as
/// "inf" or "nan", is NaN.
pub(super) fn string_to_number(s: &str) -> f64 {
    let s = s.trim_matches(is_js_whitespace);
    if s.is_empty() {
        return 0.0;
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match s.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &s[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut value = 0.0f64;
        for c in digits.chars() {
            let Some(digit) = c.to_digit(radix) else {
                return f64::NAN;
            };
            value = value * radix as f64 + digit as f64;
        }
        return value;
    }
    // StrDecimalLiteral: sign? (digits [. digits?] | . digits) exponent?
    let bytes = s.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let integer_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i - integer_start;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let fraction_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        digits += i - fraction_start;
    }
    if digits == 0 {
        return f64::NAN;
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let exponent_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            return f64::NAN;
        }
    }
    if i != bytes.len() {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// Error.prototype.toString for objects created by an Error constructor.
fn error_string(heap: &Heap, id: HeapId) -> Result<Option<String>> {
    let Some(class) = heap.class_of(id) else {
        return Ok(None);
    };
    let name = match heap.property(id, "name")? {
        VmValue::Undefined => class.to_owned(),
        value => to_string(heap, &value)?,
    };
    let message = match heap.property(id, "message")? {
        VmValue::Undefined => String::new(),
        value => to_string(heap, &value)?,
    };
    Ok(Some(match (name.is_empty(), message.is_empty()) {
        (true, _) => message,
        (false, true) => name,
        (false, false) => format!("{name}: {message}"),
    }))
}

fn to_string_inner(heap: &Heap, value: &VmValue, active: &mut BTreeSet<HeapId>) -> Result<String> {
    Ok(match value {
        VmValue::Undefined => "undefined".into(),
        VmValue::Null => "null".into(),
        VmValue::Boolean(value) => value.to_string(),
        VmValue::Number(value) => number_to_string(value.get()),
        VmValue::String(value) => value.clone(),
        VmValue::Promise(_) => "[object Promise]".into(),
        VmValue::Reference(id) => match heap.get(*id)? {
            HeapNode::Array(items) => {
                // Array.prototype.join(","): holes, null and undefined are
                // empty; a cyclic element joins as empty, as engines do.
                if !active.insert(*id) {
                    return Ok(String::new());
                }
                let mut parts = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    if !heap.has_array_index(*id, index)?
                        || matches!(item, VmValue::Null | VmValue::Undefined)
                    {
                        parts.push(String::new());
                    } else {
                        parts.push(to_string_inner(heap, item, active)?);
                    }
                }
                active.remove(id);
                parts.join(",")
            }
            HeapNode::Object(_) => {
                if let Some(text) = error_string(heap, *id)? {
                    text
                } else if heap.regex_parts(*id).is_some() {
                    let (pattern, flags) = heap.regex_parts(*id).unwrap();
                    format!("/{pattern}/{flags}")
                } else {
                    "[object Object]".into()
                }
            }
            HeapNode::Closure { function, .. } => {
                format!("function {}() {{ [flow code] }}", function_name(function))
            }
            HeapNode::Native(name) => format!("function {}() {{ [native code] }}", function_name(name)),
            HeapNode::Environment { .. } => "[object Object]".into(),
        },
    })
}

/// The `name` of a function value: "outer/inner@3:5" is "inner", "Math.max"
/// is "max", "tool:read_file" is "read_file"; anonymous functions have "".
pub(super) fn function_name(id: &str) -> &str {
    let name = id.rsplit('/').next().unwrap_or(id);
    let name = name.split('@').next().unwrap_or(name);
    let name = name.rsplit(['.', ':']).next().unwrap_or(name);
    if name.starts_with('<') { "" } else { name }
}

/// ToString, including ToPrimitive for objects and arrays.
pub(super) fn to_string(heap: &Heap, value: &VmValue) -> Result<String> {
    to_string_inner(heap, value, &mut BTreeSet::new())
}

/// ToPrimitive: objects and arrays become their string form, as for plain
/// objects whose valueOf returns the object itself.
pub(super) fn to_primitive(heap: &Heap, value: &VmValue) -> Result<VmValue> {
    Ok(match value {
        VmValue::Reference(_) | VmValue::Promise(_) => VmValue::String(to_string(heap, value)?),
        other => other.clone(),
    })
}

/// ToNumber, including ToPrimitive for objects and arrays.
pub(super) fn to_number(heap: &Heap, value: &VmValue) -> Result<f64> {
    Ok(match value {
        VmValue::Undefined => f64::NAN,
        VmValue::Null => 0.0,
        VmValue::Boolean(value) => {
            if *value {
                1.0
            } else {
                0.0
            }
        }
        VmValue::Number(value) => value.get(),
        VmValue::String(value) => string_to_number(value),
        VmValue::Reference(_) | VmValue::Promise(_) => {
            string_to_number(&to_string(heap, value)?)
        }
    })
}

/// IsStrictlyEqual: numbers by IEEE value, everything else by identity.
pub(super) fn strict_equal(a: &VmValue, b: &VmValue) -> bool {
    match (a, b) {
        (VmValue::Number(a), VmValue::Number(b)) => a.get() == b.get(),
        _ => a == b,
    }
}

/// IsLooselyEqual for the value kinds of this runtime.
pub(super) fn loose_equal(heap: &Heap, a: &VmValue, b: &VmValue) -> Result<bool> {
    use VmValue as V;
    let object = |v: &VmValue| matches!(v, V::Reference(_) | V::Promise(_));
    Ok(match (a, b) {
        (V::Undefined | V::Null, V::Undefined | V::Null) => true,
        (V::Undefined | V::Null, _) | (_, V::Undefined | V::Null) => false,
        _ if std::mem::discriminant(a) == std::mem::discriminant(b) => strict_equal(a, b),
        (V::Number(x), V::String(y)) => x.get() == string_to_number(y),
        (V::String(x), V::Number(y)) => string_to_number(x) == y.get(),
        (V::Boolean(_), _) => {
            let number = V::Number(super::heap::JsNumber::new(to_number(heap, a)?));
            loose_equal(heap, &number, b)?
        }
        (_, V::Boolean(_)) => {
            let number = V::Number(super::heap::JsNumber::new(to_number(heap, b)?));
            loose_equal(heap, a, &number)?
        }
        _ if object(a) && !object(b) => loose_equal(heap, &to_primitive(heap, a)?, b)?,
        _ if object(b) && !object(a) => loose_equal(heap, a, &to_primitive(heap, b)?)?,
        _ => false,
    })
}

/// ToIntegerOrInfinity.
pub(super) fn integer_or_infinity(number: f64) -> f64 {
    if number.is_nan() {
        0.0
    } else if number.is_infinite() {
        number
    } else {
        number.trunc()
    }
}

/// A relative start/end position (slice, splice, fill, at...), clamped to
/// [0, length].
pub(super) fn relative_position(number: f64, length: usize) -> usize {
    let relative = integer_or_infinity(number);
    if relative < 0.0 {
        (length as f64 + relative).max(0.0) as usize
    } else {
        relative.min(length as f64) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_print_like_javascript() {
        // Expected strings come from Node.js 24 String(n).
        let cases = [
            (0.0, "0"),
            (-0.0, "0"),
            (1.5, "1.5"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e-10, "1.5e-10"),
            (9007199254740992.0, "9007199254740992"),
            (0.000001, "0.000001"),
            (1e20, "100000000000000000000"),
            (-1e21, "-1e+21"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (100.0, "100"),
            (1.0 / 3.0, "0.3333333333333333"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
        ];
        for (number, expected) in cases {
            assert_eq!(number_to_string(number), expected, "{number:?}");
        }
    }

    #[test]
    fn strings_convert_to_numbers_like_javascript() {
        // Expected values come from Node.js 24 Number(s).
        let cases: [(&str, f64); 18] = [
            ("", 0.0),
            ("  12  ", 12.0),
            ("0x1A", 26.0),
            ("-0x10", f64::NAN),
            ("0b101", 5.0),
            ("0o17", 15.0),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("+Infinity", f64::INFINITY),
            ("inf", f64::NAN),
            ("nan", f64::NAN),
            ("1e3", 1000.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1_000", f64::NAN),
            (" \n\t42\n", 42.0),
            ("12px", f64::NAN),
            ("  7 \u{FEFF}", 7.0),
        ];
        for (text, expected) in cases {
            let actual = string_to_number(text);
            assert!(
                actual == expected || (actual.is_nan() && expected.is_nan()),
                "{text:?}: {actual}"
            );
        }
    }
}
