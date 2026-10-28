//! Deterministic JSON operations. No model fallback and no JavaScript runtime.
use super::*;
use anyhow::{Result, bail, ensure};

pub(super) fn text(v: &Value) -> Result<String> {
    Ok(match v {
        Value::String(s) => s.clone(),
        Value::Null | Value::Bool(_) | Value::Number(_) => v.to_string(),
        _ => {
            bail!(
                "string concatenation/template/join values must be JSON primitives; use JSON.stringify for containers"
            )
        }
    })
}
const REGEX_PATTERN_LIMIT: usize = 8 * 1024;

pub(super) fn compile_regex(pattern: &str, flags: &str) -> Result<regex::Regex> {
    ensure!(
        pattern.len() <= REGEX_PATTERN_LIMIT,
        "regex pattern exceeds 8192 bytes"
    );
    let mut seen = String::new();
    for flag in flags.chars() {
        ensure!(
            matches!(flag, 'g' | 'i' | 'm' | 's' | 'u'),
            "unsupported regex flag: {flag}"
        );
        ensure!(!seen.contains(flag), "duplicate regex flag: {flag}");
        seen.push(flag);
    }
    let normalized = translate_js_pattern(pattern, flags.contains('s'));
    let mut builder = regex::RegexBuilder::new(&normalized);
    builder
        .case_insensitive(flags.contains('i'))
        .multi_line(flags.contains('m'))
        .dot_matches_new_line(flags.contains('s'))
        .size_limit(1024 * 1024);
    Ok(builder.build()?)
}

/// JavaScript character classes, written as Rust regex syntax.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";
const JS_WORD: &str = "0-9A-Za-z_";

/// Translate a JavaScript pattern to Rust regex syntax with JavaScript's
/// meaning: \d, \w and \b are ASCII; \s is the ECMAScript WhiteSpace and
/// LineTerminator set; `.` excludes all line terminators unless the s flag is
/// set. Inside a class, `[`, `&` and `~` are literal in JavaScript but are
/// nesting and set operators in Rust, so they are escaped.
pub(super) fn translate_js_pattern(pattern: &str, dot_all: bool) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let chars: Vec<char> = pattern.chars().collect();
    let mut in_class = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                let Some(&next) = chars.get(i + 1) else {
                    out.push_str(r"\\");
                    break;
                };
                i += 1;
                match (next, in_class) {
                    ('/', _) => out.push('/'),
                    ('d', false) => out.push_str("[0-9]"),
                    ('d', true) => out.push_str("0-9"),
                    ('D', _) => out.push_str("[^0-9]"),
                    ('w', false) => out.push_str(&format!("[{JS_WORD}]")),
                    ('w', true) => out.push_str(JS_WORD),
                    ('W', _) => out.push_str(&format!("[^{JS_WORD}]")),
                    ('s', false) => out.push_str(&format!("[{JS_SPACE}]")),
                    ('s', true) => out.push_str(JS_SPACE),
                    ('S', _) => out.push_str(&format!("[^{JS_SPACE}]")),
                    ('b', false) => out.push_str(r"(?-u:\b)"),
                    ('B', false) => out.push_str(r"(?-u:\B)"),
                    ('b', true) => out.push_str(r"\x08"),
                    ('0', _) if !chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) => {
                        out.push_str(r"\x00")
                    }
                    ('c', _) if chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) => {
                        i += 1;
                        out.push_str(&format!(r"\x{:02X}", (chars[i] as u32) % 32));
                    }
                    (other, _) => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            '[' if !in_class => {
                // `[]` matches nothing and `[^]` matches anything in JavaScript.
                if chars.get(i + 1) == Some(&']') {
                    out.push_str(r"[^\x{0}-\x{10FFFF}]");
                    i += 1;
                } else if chars.get(i + 1) == Some(&'^') && chars.get(i + 2) == Some(&']') {
                    out.push_str(r"[\x{0}-\x{10FFFF}]");
                    i += 2;
                } else {
                    in_class = true;
                    out.push('[');
                    if chars.get(i + 1) == Some(&'^') {
                        out.push('^');
                        i += 1;
                    }
                    // A leading `]` is literal in neither language; JavaScript
                    // closed the class above, so nothing else is needed here.
                }
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            '[' | '&' | '~' if in_class => {
                out.push('\\');
                out.push(c);
            }
            '.' if !in_class && !dot_all => out.push_str(r"[^\n\r\x{2028}\x{2029}]"),
            '{' if !in_class && !is_quantifier(&chars[i..]) => out.push_str(r"\{"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// `{n}`, `{n,}` or `{n,m}` at the start of `rest`.
fn is_quantifier(rest: &[char]) -> bool {
    let mut i = 1;
    let start = i;
    while rest.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    if i == start {
        return false;
    }
    if rest.get(i) == Some(&',') {
        i += 1;
        while rest.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
    }
    rest.get(i) == Some(&'}')
}

fn regex_parts(value: &Value) -> Option<(&str, &str)> {
    let object = value.as_object()?;
    if object.len() != 1 {
        return None;
    }
    let descriptor = object.get("$flow_regex")?.as_object()?;
    Some((
        descriptor.get("pattern")?.as_str()?,
        descriptor.get("flags")?.as_str()?,
    ))
}

pub(super) fn regexp(args: Vec<Value>) -> Result<Value> {
    let original = args.first().and_then(regex_parts);
    let pattern = match args.first() {
        None => String::new(),
        Some(_) if original.is_some() => original.unwrap().0.to_owned(),
        Some(value) => text(value)?,
    };
    let flags = match args.get(1) {
        Some(value) => text(value)?,
        None => original.map(|(_, flags)| flags.to_owned()).unwrap_or_default(),
    };
    compile_regex(&pattern, &flags)?;
    Ok(json!({"$flow_regex": {"pattern": pattern, "flags": flags}}))
}

