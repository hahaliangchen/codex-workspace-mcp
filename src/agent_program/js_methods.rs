//! Built-in methods and global functions over VM values, following the
//! ECMAScript algorithms. Strings are indexed by Unicode scalar values (a
//! documented difference from UTF-16 code units); everything else matches the
//! specification for the value kinds this runtime has.
use super::heap::{Heap, HeapId, HeapNode, JsNumber, VmValue};
use super::js_value::{
    integer_or_infinity, is_js_whitespace, number_to_string, relative_position, strict_equal,
    string_to_number, to_number, to_string,
};
use anyhow::{Result, bail, ensure};
use std::collections::BTreeSet;

const TEXT_RESULT_LIMIT: usize = 4 * 1024 * 1024;
const ARRAY_LIMIT: usize = 1_000_000;
const REGEX_RESULT_LIMIT: usize = 10_000;

fn arg(args: &[VmValue], index: usize) -> VmValue {
    args.get(index).cloned().unwrap_or(VmValue::Undefined)
}
fn number(value: f64) -> VmValue {
    VmValue::Number(JsNumber::new(value))
}
fn text_result(value: String) -> Result<VmValue> {
    ensure!(value.len() <= TEXT_RESULT_LIMIT, "RangeError: string result exceeds 4 MiB");
    Ok(VmValue::String(value))
}
fn new_array(heap: &mut Heap, items: Vec<VmValue>) -> Result<VmValue> {
    ensure!(items.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
    Ok(VmValue::Reference(heap.allocate(HeapNode::Array(items))?))
}
fn array_items(heap: &Heap, id: HeapId) -> Result<Vec<VmValue>> {
    match heap.get(id)? {
        HeapNode::Array(items) => Ok(items.clone()),
        _ => bail!("TypeError: receiver is not an array"),
    }
}
/// SameValueZero, used by includes.
fn same_value_zero(a: &VmValue, b: &VmValue) -> bool {
    match (a, b) {
        (VmValue::Number(a), VmValue::Number(b)) if a.get().is_nan() && b.get().is_nan() => true,
        _ => strict_equal(a, b),
    }
}

/// Method names a compiled program may call, with their argument counts.
/// The receiver kind is only known at run time, so this is the union.
pub(super) fn method_arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "at" | "repeat" | "search" | "match" | "matchAll" | "test" | "exec" => (1, 1),
        "charAt" | "charCodeAt" | "codePointAt" | "toFixed" | "join" | "flat" | "toString"
        | "toLowerCase" | "toUpperCase" | "toLocaleLowerCase" | "toLocaleUpperCase"
        | "toLocaleString" => (0, 1),
        "includes" | "indexOf" | "lastIndexOf" | "startsWith" | "endsWith" | "padStart"
        | "padEnd" => (1, 2),
        "slice" | "split" | "substring" | "substr" => (0, 2),
        "replace" | "replaceAll" => (2, 2),
        "trim" | "trimStart" | "trimEnd" | "valueOf" | "pop" | "shift" | "reverse" | "keys"
        | "values" | "entries" | "toReversed" => (0, 0),
        "concat" | "push" | "unshift" | "splice" => (0, 64),
        "fill" => (1, 3),
        "sort" | "toSorted" => (0, 1),
        _ => return None,
    })
}

/// Explain unavailable methods that a model is likely to reach for.
pub(super) fn unavailable_method(name: &str) -> Option<&'static str> {
    Some(match name {
        "localeCompare" => "localeCompare depends on locale collation data that the flow runtime does not have; compare strings with < and > (code point order)",
        "normalize" => "String.prototype.normalize is unavailable in the flow runtime",
        _ => return None,
    })
}

fn regex_parts(heap: &Heap, value: &VmValue) -> Option<(String, String)> {
    match value {
        VmValue::Reference(id) => heap.regex_parts(*id),
        _ => None,
    }
}

/// Byte offsets from the regex engine become Unicode scalar positions.
fn char_index(text: &str, byte: usize) -> usize {
    text[..byte].chars().count()
}

/// GetSubstitution: $$, $&, $`, $', $n, $nn and $<name> in a replacement string.
fn substitution(
    replacement: &str,
    text: &str,
    start: usize,
    end: usize,
    captures: &[Option<String>],
    names: &[(String, Option<String>)],
) -> String {
    let matched = &text[start..end];
    let chars: Vec<char> = replacement.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' || i + 1 == chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let next = chars[i + 1];
        match next {
            '$' => {
                out.push('$');
                i += 2;
            }
            '&' => {
                out.push_str(matched);
                i += 2;
            }
            '`' => {
                out.push_str(&text[..start]);
                i += 2;
            }
            '\'' => {
                out.push_str(&text[end..]);
                i += 2;
            }
            '0'..='9' => {
                let first = next.to_digit(10).unwrap() as usize;
                let second = chars.get(i + 2).and_then(|c| c.to_digit(10)).map(|d| d as usize);
                let count = captures.len();
                if let Some(second) = second
                    && (1..=count).contains(&(first * 10 + second))
                {
                    out.push_str(captures[first * 10 + second - 1].as_deref().unwrap_or(""));
                    i += 3;
                } else if (1..=count).contains(&first) {
                    out.push_str(captures[first - 1].as_deref().unwrap_or(""));
                    i += 2;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            '<' if !names.is_empty() => {
                let rest: String = chars[i + 2..].iter().collect();
                if let Some(close) = rest.find('>') {
                    let name = &rest[..close];
                    let value = names
                        .iter()
                        .find(|(group, _)| group == name)
                        .and_then(|(_, value)| value.clone())
                        .unwrap_or_default();
                    out.push_str(&value);
                    i += 2 + name.chars().count() + 1;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

fn captures_of(captures: &regex::Captures<'_>) -> Vec<Option<String>> {
    captures
        .iter()
        .skip(1)
        .map(|m| m.map(|m| m.as_str().to_owned()))
        .collect()
}
fn names_of(re: &regex::Regex, captures: &regex::Captures<'_>) -> Vec<(String, Option<String>)> {
    re.capture_names()
        .flatten()
        .map(|name| (name.to_owned(), captures.name(name).map(|m| m.as_str().to_owned())))
        .collect()
}

/// The match array of RegExpBuiltinExec, with undefined for unmatched groups.
fn match_array(heap: &mut Heap, captures: &regex::Captures<'_>) -> Result<VmValue> {
    ensure!(
        captures.len() <= REGEX_RESULT_LIMIT,
        "RangeError: regex capture result exceeds 10000 items"
    );
    let items = captures
        .iter()
        .map(|m| match m {
            Some(m) => VmValue::String(m.as_str().to_owned()),
            None => VmValue::Undefined,
        })
        .collect();
    new_array(heap, items)
}

/// A string pattern used where a RegExp is expected becomes `new RegExp(pattern)`.
fn regex_argument(heap: &Heap, value: &VmValue) -> Result<(regex::Regex, String)> {
    let (pattern, flags) = match regex_parts(heap, value) {
        Some(parts) => parts,
        None => (to_string(heap, value)?, String::new()),
    };
    Ok((super::values::compile_regex(&pattern, &flags)?, flags))
}

fn replace(heap: &mut Heap, text: &str, name: &str, args: &[VmValue]) -> Result<VmValue> {
    let pattern = arg(args, 0);
    let replacement = arg(args, 1);
    if matches!(replacement, VmValue::Reference(id) if matches!(heap.get(id), Ok(HeapNode::Closure { .. })))
    {
        bail!("TypeError: replacement functions are not available yet; compute the replacement with a loop over matches");
    }
    let replacement = to_string(heap, &replacement)?;
    let mut out = String::new();
    if let Some((source, flags)) = regex_parts(heap, &pattern) {
        let global = flags.contains('g');
        ensure!(
            name == "replace" || global,
            "TypeError: replaceAll must be called with a global RegExp"
        );
        let re = super::values::compile_regex(&source, &flags)?;
        let mut last = 0;
        for captures in re.captures_iter(text) {
            let whole = captures.get(0).unwrap();
            out.push_str(&text[last..whole.start()]);
            out.push_str(&substitution(
                &replacement,
                text,
                whole.start(),
                whole.end(),
                &captures_of(&captures),
                &names_of(&re, &captures),
            ));
            last = whole.end();
            ensure!(out.len() <= TEXT_RESULT_LIMIT, "RangeError: string result exceeds 4 MiB");
            if !global {
                break;
            }
        }
        out.push_str(&text[last..]);
        return text_result(out);
    }
    let search = to_string(heap, &pattern)?;
    let mut positions = Vec::new();
    if name == "replace" {
        if let Some(position) = text.find(&search) {
            positions.push(position);
        }
    } else if search.is_empty() {
        positions.push(0);
        positions.extend(text.char_indices().map(|(i, c)| i + c.len_utf8()));
    } else {
        positions.extend(text.match_indices(&search).map(|(i, _)| i));
    }
    let mut last = 0;
    for position in positions {
        out.push_str(&text[last..position]);
        out.push_str(&substitution(&replacement, text, position, position + search.len(), &[], &[]));
        last = position + search.len();
        ensure!(out.len() <= TEXT_RESULT_LIMIT, "RangeError: string result exceeds 4 MiB");
    }
    out.push_str(&text[last..]);
    text_result(out)
}

fn split(heap: &mut Heap, text: &str, args: &[VmValue]) -> Result<VmValue> {
    let separator = arg(args, 0);
    let limit = match arg(args, 1) {
        VmValue::Undefined => u32::MAX as usize,
        value => {
            let n = to_number(heap, &value)?;
            if n.is_finite() { (n.trunc().rem_euclid(4294967296.0)) as usize } else { 0 }
        }
    };
    let mut parts: Vec<VmValue> = Vec::new();
    if limit == 0 {
        return new_array(heap, parts);
    }
    if matches!(separator, VmValue::Undefined) {
        return new_array(heap, vec![VmValue::String(text.to_owned())]);
    }
    if let Some((source, flags)) = regex_parts(heap, &separator) {
        let re = super::values::compile_regex(&source, &flags)?;
        if text.is_empty() {
            return new_array(
                heap,
                if re.is_match("") { vec![] } else { vec![VmValue::String(String::new())] },
            );
        }
        let mut last = 0;
        for captures in re.captures_iter(text) {
            let whole = captures.get(0).unwrap();
            // An empty match at the previous split point or at the end does
            // not split, exactly as the specification's SplitMatcher loop.
            if whole.start() == whole.end() && (whole.start() == last || whole.start() == text.len()) {
                continue;
            }
            if whole.end() == whole.start() && whole.start() >= text.len() {
                continue;
            }
            parts.push(VmValue::String(text[last..whole.start()].to_owned()));
            if parts.len() == limit {
                return new_array(heap, parts);
            }
            for capture in captures.iter().skip(1) {
                parts.push(match capture {
                    Some(m) => VmValue::String(m.as_str().to_owned()),
                    None => VmValue::Undefined,
                });
                if parts.len() == limit {
                    return new_array(heap, parts);
                }
            }
            last = whole.end();
            ensure!(parts.len() <= ARRAY_LIMIT, "RangeError: split result exceeds the runtime limit");
        }
        parts.push(VmValue::String(text[last..].to_owned()));
        return new_array(heap, parts);
    }
    let separator = to_string(heap, &separator)?;
    let pieces: Vec<String> = if separator.is_empty() {
        text.chars().map(|c| c.to_string()).collect()
    } else {
        text.split(separator.as_str()).map(str::to_owned).collect()
    };
    parts.extend(pieces.into_iter().take(limit).map(VmValue::String));
    new_array(heap, parts)
}

/// String.prototype methods. Positions count Unicode scalar values.
fn string_method(heap: &mut Heap, text: &str, name: &str, args: &[VmValue]) -> Result<VmValue> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let position = |heap: &Heap, value: &VmValue, default: usize| -> Result<usize> {
        Ok(match value {
            VmValue::Undefined => default,
            value => integer_or_infinity(to_number(heap, value)?).clamp(0.0, len as f64) as usize,
        })
    };
    let search_string = |heap: &Heap, value: &VmValue| -> Result<Vec<char>> {
        ensure!(
            regex_parts(heap, value).is_none(),
            "TypeError: First argument to String.prototype.{name} must not be a regular expression"
        );
        Ok(to_string(heap, value)?.chars().collect())
    };
    let find_from = |needle: &[char], from: usize| -> Option<usize> {
        if needle.is_empty() {
            return Some(from.min(len));
        }
        (from..len).find(|&i| chars[i..].starts_with(needle))
    };
    Ok(match name {
        "at" => {
            let relative = integer_or_infinity(to_number(heap, &arg(args, 0))?);
            let index = if relative < 0.0 { len as f64 + relative } else { relative };
            if index < 0.0 || index >= len as f64 {
                VmValue::Undefined
            } else {
                VmValue::String(chars[index as usize].to_string())
            }
        }
        "charAt" | "charCodeAt" | "codePointAt" => {
            let index = integer_or_infinity(to_number(heap, &arg(args, 0))?);
            let found = (index >= 0.0 && index < len as f64).then(|| chars[index as usize]);
            match (name, found) {
                ("charAt", Some(c)) => VmValue::String(c.to_string()),
                ("charAt", None) => VmValue::String(String::new()),
                ("charCodeAt", Some(c)) => {
                    let mut units = [0u16; 2];
                    number(c.encode_utf16(&mut units)[0] as f64)
                }
                ("charCodeAt", None) => number(f64::NAN),
                (_, Some(c)) => number(c as u32 as f64),
                (_, None) => VmValue::Undefined,
            }
        }
        "concat" => {
            let mut out = text.to_owned();
            for value in args {
                out.push_str(&to_string(heap, value)?);
            }
            text_result(out)?
        }
        "includes" | "startsWith" => {
            let needle = search_string(heap, &arg(args, 0))?;
            let start = position(heap, &arg(args, 1), 0)?;
            VmValue::Boolean(if name == "startsWith" {
                chars[start..].starts_with(&needle)
            } else {
                find_from(&needle, start).is_some()
            })
        }
        "endsWith" => {
            let needle = search_string(heap, &arg(args, 0))?;
            let end = position(heap, &arg(args, 1), len)?;
            VmValue::Boolean(chars[..end].ends_with(&needle))
        }
        "indexOf" => {
            let needle: Vec<char> = to_string(heap, &arg(args, 0))?.chars().collect();
            let start = position(heap, &arg(args, 1), 0)?;
            number(find_from(&needle, start).map(|i| i as f64).unwrap_or(-1.0))
        }
        "lastIndexOf" => {
            let needle: Vec<char> = to_string(heap, &arg(args, 0))?.chars().collect();
            let from = match arg(args, 1) {
                VmValue::Undefined => f64::INFINITY,
                value => {
                    let n = to_number(heap, &value)?;
                    if n.is_nan() { f64::INFINITY } else { n.trunc() }
                }
            };
            let start = from.clamp(0.0, len as f64) as usize;
            let last = start.min(len.saturating_sub(needle.len()));
            let found = if needle.len() > len {
                None
            } else {
                (0..=last).rev().find(|&i| chars[i..].starts_with(&needle))
            };
            number(found.map(|i| i as f64).unwrap_or(-1.0))
        }
        "padStart" | "padEnd" => {
            let target = to_number(heap, &arg(args, 0))?;
            let target = if target.is_nan() { 0.0 } else { target.trunc() };
            let fill = match arg(args, 1) {
                VmValue::Undefined => " ".to_owned(),
                value => to_string(heap, &value)?,
            };
            if target <= len as f64 || fill.is_empty() {
                VmValue::String(text.to_owned())
            } else {
                ensure!(target <= TEXT_RESULT_LIMIT as f64, "RangeError: Invalid string length");
                let needed = target as usize - len;
                let padding: String = fill.chars().cycle().take(needed).collect();
                text_result(if name == "padStart" {
                    format!("{padding}{text}")
                } else {
                    format!("{text}{padding}")
                })?
            }
        }
        "repeat" => {
            let count = integer_or_infinity(to_number(heap, &arg(args, 0))?);
            ensure!(count >= 0.0 && count.is_finite(), "RangeError: Invalid count value: {count}");
            ensure!(
                (text.len() as f64) * count <= TEXT_RESULT_LIMIT as f64,
                "RangeError: Invalid string length"
            );
            VmValue::String(text.repeat(count as usize))
        }
        "replace" | "replaceAll" => replace(heap, text, name, args)?,
        "search" => {
            let (re, _) = regex_argument(heap, &arg(args, 0))?;
            number(re.find(text).map(|m| char_index(text, m.start()) as f64).unwrap_or(-1.0))
        }
        "match" => {
            let value = arg(args, 0);
            let (re, flags) = regex_argument(heap, &value)?;
            if flags.contains('g') {
                let mut matches = Vec::new();
                for m in re.find_iter(text) {
                    ensure!(matches.len() < REGEX_RESULT_LIMIT, "RangeError: regex match result exceeds 10000 items");
                    matches.push(VmValue::String(m.as_str().to_owned()));
                }
                if matches.is_empty() { VmValue::Null } else { new_array(heap, matches)? }
            } else {
                match re.captures(text) {
                    Some(captures) => match_array(heap, &captures)?,
                    None => VmValue::Null,
                }
            }
        }
        "matchAll" => {
            let value = arg(args, 0);
            let (re, flags) = regex_argument(heap, &value)?;
            ensure!(
                regex_parts(heap, &value).is_none() || flags.contains('g'),
                "TypeError: String.prototype.matchAll called with a non-global RegExp argument"
            );
            let mut all = Vec::new();
            for captures in re.captures_iter(text) {
                ensure!(all.len() < REGEX_RESULT_LIMIT, "RangeError: regex match result exceeds 10000 items");
                all.push(match_array(heap, &captures)?);
            }
            new_array(heap, all)?
        }
        "slice" => {
            let start = match arg(args, 0) {
                VmValue::Undefined => 0,
                value => relative_position(to_number(heap, &value)?, len),
            };
            let end = match arg(args, 1) {
                VmValue::Undefined => len,
                value => relative_position(to_number(heap, &value)?, len),
            };
            VmValue::String(chars[start..end.max(start)].iter().collect())
        }
        "substring" => {
            let start = position(heap, &arg(args, 0), 0)?;
            let end = position(heap, &arg(args, 1), len)?;
            let (from, to) = (start.min(end), start.max(end));
            VmValue::String(chars[from..to].iter().collect())
        }
        "substr" => {
            let start = match arg(args, 0) {
                VmValue::Undefined => 0,
                value => relative_position(to_number(heap, &value)?, len),
            };
            let count = match arg(args, 1) {
                VmValue::Undefined => len - start,
                value => integer_or_infinity(to_number(heap, &value)?).clamp(0.0, (len - start) as f64) as usize,
            };
            VmValue::String(chars[start..start + count].iter().collect())
        }
        "split" => split(heap, text, args)?,
        "toLowerCase" | "toLocaleLowerCase" => VmValue::String(text.to_lowercase()),
        "toUpperCase" | "toLocaleUpperCase" => VmValue::String(text.to_uppercase()),
        "toString" | "valueOf" => VmValue::String(text.to_owned()),
        "trim" => VmValue::String(text.trim_matches(is_js_whitespace).to_owned()),
        "trimStart" => VmValue::String(text.trim_start_matches(is_js_whitespace).to_owned()),
        "trimEnd" => VmValue::String(text.trim_end_matches(is_js_whitespace).to_owned()),
        _ => bail!("TypeError: string.{name} is not a function"),
    })
}

/// The default sort order: undefined last, otherwise by string value in
/// UTF-16 code unit order. The sort is stable, as the specification requires.
fn default_sort(heap: &Heap, items: Vec<VmValue>) -> Result<Vec<VmValue>> {
    let mut keyed = Vec::with_capacity(items.len());
    let mut undefined = 0;
    for item in items {
        if matches!(item, VmValue::Undefined) {
            undefined += 1;
        } else {
            let key: Vec<u16> = to_string(heap, &item)?.encode_utf16().collect();
            keyed.push((key, item));
        }
    }
    keyed.sort_by(|a, b| a.0.cmp(&b.0));
    let mut sorted: Vec<VmValue> = keyed.into_iter().map(|(_, item)| item).collect();
    sorted.extend(std::iter::repeat_n(VmValue::Undefined, undefined));
    Ok(sorted)
}

/// Array.prototype methods that take no callback. Callback methods are
/// compiled into the calling frame so that callbacks can suspend.
fn array_method(heap: &mut Heap, id: HeapId, name: &str, args: &[VmValue]) -> Result<VmValue> {
    let items = array_items(heap, id)?;
    let holes = heap.holes(id);
    let len = items.len();
    let present = |index: usize| !holes.contains(&index);
    Ok(match name {
        "at" => {
            let relative = integer_or_infinity(to_number(heap, &arg(args, 0))?);
            let index = if relative < 0.0 { len as f64 + relative } else { relative };
            if index < 0.0 || index >= len as f64 || !present(index as usize) {
                VmValue::Undefined
            } else {
                items[index as usize].clone()
            }
        }
        "concat" => {
            let mut out = items.clone();
            let mut out_holes: BTreeSet<usize> = holes.clone();
            for value in args {
                match value {
                    VmValue::Reference(other) if matches!(heap.get(*other)?, HeapNode::Array(_)) => {
                        let offset = out.len();
                        let other_holes = heap.holes(*other);
                        out.extend(array_items(heap, *other)?);
                        out_holes.extend(other_holes.into_iter().map(|i| i + offset));
                    }
                    other => out.push(other.clone()),
                }
            }
            ensure!(out.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
            let result = heap.allocate(HeapNode::Array(Vec::new()))?;
            heap.replace_array(result, out, out_holes)?;
            VmValue::Reference(result)
        }
        "includes" | "indexOf" => {
            let needle = arg(args, 0);
            let start = match arg(args, 1) {
                VmValue::Undefined => 0,
                value => relative_position(to_number(heap, &value)?, len),
            };
            let found = (start..len).find(|&i| {
                if name == "includes" {
                    let value = if present(i) { &items[i] } else { &VmValue::Undefined };
                    same_value_zero(value, &needle)
                } else {
                    present(i) && strict_equal(&items[i], &needle)
                }
            });
            if name == "includes" {
                VmValue::Boolean(found.is_some())
            } else {
                number(found.map(|i| i as f64).unwrap_or(-1.0))
            }
        }
        "lastIndexOf" => {
            let needle = arg(args, 0);
            if len == 0 {
                return Ok(number(-1.0));
            }
            let from = match arg(args, 1) {
                VmValue::Undefined => len as f64 - 1.0,
                value => integer_or_infinity(to_number(heap, &value)?),
            };
            let start = if from < 0.0 { len as f64 + from } else { from.min(len as f64 - 1.0) };
            if start < 0.0 {
                number(-1.0)
            } else {
                let found = (0..=start as usize).rev().find(|&i| present(i) && strict_equal(&items[i], &needle));
                number(found.map(|i| i as f64).unwrap_or(-1.0))
            }
        }
        "join" | "toString" => {
            let separator = match (name, arg(args, 0)) {
                ("toString", _) | (_, VmValue::Undefined) => ",".to_owned(),
                (_, value) => to_string(heap, &value)?,
            };
            let mut parts = Vec::with_capacity(len);
            for (index, item) in items.iter().enumerate() {
                parts.push(if !present(index) || matches!(item, VmValue::Undefined | VmValue::Null) {
                    String::new()
                } else {
                    to_string(heap, item)?
                });
            }
            text_result(parts.join(&separator))?
        }
        "slice" => {
            let start = match arg(args, 0) {
                VmValue::Undefined => 0,
                value => relative_position(to_number(heap, &value)?, len),
            };
            let end = match arg(args, 1) {
                VmValue::Undefined => len,
                value => relative_position(to_number(heap, &value)?, len),
            };
            let end = end.max(start);
            let result = heap.allocate(HeapNode::Array(Vec::new()))?;
            let slice_holes = holes.iter().filter(|i| **i >= start && **i < end).map(|i| i - start).collect();
            heap.replace_array(result, items[start..end].to_vec(), slice_holes)?;
            VmValue::Reference(result)
        }
        "push" => {
            ensure!(len + args.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
            number(heap.append(id, args.to_vec())? as f64)
        }
        "pop" | "shift" => {
            if len == 0 {
                return Ok(VmValue::Undefined);
            }
            let index = if name == "pop" { len - 1 } else { 0 };
            let removed = if present(index) { items[index].clone() } else { VmValue::Undefined };
            let mut rest = items;
            rest.remove(index);
            let rest_holes = holes
                .iter()
                .filter(|i| **i != index)
                .map(|i| if name == "shift" { i - 1 } else { *i })
                .collect();
            heap.replace_array(id, rest, rest_holes)?;
            removed
        }
        "unshift" => {
            ensure!(len + args.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
            let mut out = args.to_vec();
            let shifted = holes.iter().map(|i| i + args.len()).collect();
            out.extend(items);
            let length = out.len();
            heap.replace_array(id, out, shifted)?;
            number(length as f64)
        }
        "splice" => {
            if args.is_empty() {
                return new_array(heap, Vec::new());
            }
            let start = relative_position(to_number(heap, &args[0])?, len);
            let delete = if args.len() == 1 {
                len - start
            } else {
                integer_or_infinity(to_number(heap, &args[1])?).clamp(0.0, (len - start) as f64) as usize
            };
            let inserted = args.get(2..).unwrap_or(&[]).to_vec();
            ensure!(len - delete + inserted.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
            let removed: Vec<VmValue> = items[start..start + delete].to_vec();
            let removed_holes = holes.iter().filter(|i| **i >= start && **i < start + delete).map(|i| i - start).collect();
            let mut out = items[..start].to_vec();
            out.extend(inserted.iter().cloned());
            out.extend(items[start + delete..].iter().cloned());
            let shift = inserted.len() as isize - delete as isize;
            let out_holes = holes
                .iter()
                .filter(|i| **i < start || **i >= start + delete)
                .map(|i| if *i < start { *i } else { (*i as isize + shift) as usize })
                .collect();
            heap.replace_array(id, out, out_holes)?;
            let result = heap.allocate(HeapNode::Array(Vec::new()))?;
            heap.replace_array(result, removed, removed_holes)?;
            VmValue::Reference(result)
        }
        "reverse" | "toReversed" => {
            let mut out = items;
            out.reverse();
            let mirrored: BTreeSet<usize> = holes.iter().map(|i| len - 1 - i).collect();
            if name == "reverse" {
                heap.replace_array(id, out, mirrored)?;
                VmValue::Reference(id)
            } else {
                let out = out
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| if mirrored.contains(&i) { VmValue::Undefined } else { v })
                    .collect();
                new_array(heap, out)?
            }
        }
        "fill" => {
            let value = arg(args, 0);
            let start = match arg(args, 1) {
                VmValue::Undefined => 0,
                v => relative_position(to_number(heap, &v)?, len),
            };
            let end = match arg(args, 2) {
                VmValue::Undefined => len,
                v => relative_position(to_number(heap, &v)?, len),
            };
            let mut out = items;
            for item in out.iter_mut().take(end).skip(start) {
                *item = value.clone();
            }
            let kept = holes.iter().filter(|i| **i < start || **i >= end).copied().collect();
            heap.replace_array(id, out, kept)?;
            VmValue::Reference(id)
        }
        "flat" => {
            let depth = match arg(args, 0) {
                VmValue::Undefined => 1.0,
                value => integer_or_infinity(to_number(heap, &value)?),
            };
            fn flatten(heap: &Heap, id: HeapId, depth: f64, out: &mut Vec<VmValue>) -> Result<()> {
                let holes = heap.holes(id);
                for (index, item) in array_items(heap, id)?.into_iter().enumerate() {
                    if holes.contains(&index) {
                        continue;
                    }
                    match item {
                        VmValue::Reference(inner) if depth >= 1.0 && matches!(heap.get(inner)?, HeapNode::Array(_)) => {
                            flatten(heap, inner, depth - 1.0, out)?
                        }
                        other => out.push(other),
                    }
                    ensure!(out.len() <= ARRAY_LIMIT, "RangeError: array exceeds the runtime limit");
                }
                Ok(())
            }
            let mut out = Vec::new();
            flatten(heap, id, depth, &mut out)?;
            new_array(heap, out)?
        }
        "keys" => new_array(heap, (0..len).map(|i| number(i as f64)).collect())?,
        "values" => new_array(
            heap,
            items.iter().enumerate().map(|(i, v)| if present(i) { v.clone() } else { VmValue::Undefined }).collect(),
        )?,
        "entries" => {
            let mut out = Vec::with_capacity(len);
            for (index, item) in items.iter().enumerate() {
                let value = if present(index) { item.clone() } else { VmValue::Undefined };
                out.push(new_array(heap, vec![number(index as f64), value])?);
            }
            new_array(heap, out)?
        }
        "sort" | "toSorted" => {
            ensure!(
                matches!(arg(args, 0), VmValue::Undefined),
                "TypeError: comparator functions are compiled at the call site; pass an arrow function directly"
            );
            let values: Vec<VmValue> = items
                .iter()
                .enumerate()
                .filter(|(i, _)| present(*i))
                .map(|(_, v)| v.clone())
                .collect();
            let present_count = values.len();
            let sorted = default_sort(heap, values)?;
            if name == "sort" {
                let mut out = sorted;
                out.resize(len, VmValue::Undefined);
                let tail = (present_count..len).collect();
                heap.replace_array(id, out, tail)?;
                VmValue::Reference(id)
            } else {
                let mut out = sorted;
                out.resize(len, VmValue::Undefined);
                new_array(heap, out)?
            }
        }
        _ => bail!("TypeError: array.{name} is not a function"),
    })
}

/// Number.prototype.toFixed: the exact binary value rounded half up, as the
/// specification's "larger n" rule requires.
fn to_fixed(value: f64, digits: usize) -> String {
    if !value.is_finite() || value.abs() >= 1e21 {
        return number_to_string(value);
    }
    let negative = value < 0.0;
    // A double has at most 1074 fractional digits, so this expansion is exact.
    let exact = format!("{:.1100}", value.abs());
    let (whole, fraction) = exact.split_once('.').unwrap();
    let mut digits_out: Vec<u8> = whole.bytes().chain(fraction.bytes().take(digits)).map(|b| b - b'0').collect();
    let round_up = fraction.as_bytes().get(digits).is_some_and(|b| *b >= b'5');
    if round_up {
        let mut i = digits_out.len();
        loop {
            if i == 0 {
                digits_out.insert(0, 1);
                break;
            }
            i -= 1;
            if digits_out[i] == 9 {
                digits_out[i] = 0;
            } else {
                digits_out[i] += 1;
                break;
            }
        }
    }
    let text: String = digits_out.iter().map(|d| (b'0' + d) as char).collect();
    let split = text.len() - digits;
    let body = if digits == 0 {
        text
    } else {
        format!("{}.{}", &text[..split], &text[split..])
    };
    let is_zero = body.chars().all(|c| c == '0' || c == '.');
    if negative && !is_zero { format!("-{body}") } else { body }
}

fn radix_string(value: f64, radix: u32) -> String {
    if radix == 10 || !value.is_finite() {
        return number_to_string(value);
    }
    let negative = value < 0.0;
    let mut integer = value.abs().trunc();
    let mut fraction = value.abs() - integer;
    let mut digits = Vec::new();
    if integer == 0.0 {
        digits.push('0');
    }
    while integer >= 1.0 {
        let digit = (integer % radix as f64) as u32;
        digits.push(std::char::from_digit(digit, radix).unwrap());
        integer = (integer / radix as f64).trunc();
    }
    digits.reverse();
    let mut out: String = digits.into_iter().collect();
    if fraction > 0.0 {
        out.push('.');
        for _ in 0..52 {
            fraction *= radix as f64;
            let digit = fraction.trunc() as u32;
            out.push(std::char::from_digit(digit, radix).unwrap());
            fraction -= digit as f64;
            if fraction == 0.0 {
                break;
            }
        }
    }
    if negative { format!("-{out}") } else { out }
}

/// Number.prototype.toLocaleString for the en/zh grouping this host uses:
/// comma groups of three and at most three fraction digits, half expanded.
fn locale_string(value: f64) -> String {
    if !value.is_finite() {
        return if value.is_nan() { "NaN".into() } else if value > 0.0 { "∞".into() } else { "-∞".into() };
    }
    let fixed = to_fixed(value, 3);
    let (whole, fraction) = fixed.split_once('.').unwrap_or((&fixed, ""));
    let (sign, whole) = whole.strip_prefix('-').map(|w| ("-", w)).unwrap_or(("", whole));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{fraction}")
    }
}

fn number_method(heap: &mut Heap, value: f64, name: &str, args: &[VmValue]) -> Result<VmValue> {
    Ok(match name {
        "toFixed" => {
            let digits = integer_or_infinity(to_number(heap, &arg(args, 0))?);
            ensure!((0.0..=100.0).contains(&digits), "RangeError: toFixed() digits argument must be between 0 and 100");
            VmValue::String(to_fixed(value, digits as usize))
        }
        "toString" => {
            let radix = match arg(args, 0) {
                VmValue::Undefined => 10.0,
                radix => integer_or_infinity(to_number(heap, &radix)?),
            };
            ensure!((2.0..=36.0).contains(&radix), "RangeError: toString() radix must be between 2 and 36");
            VmValue::String(radix_string(value, radix as u32))
        }
        "toLocaleString" => {
            match arg(args, 0) {
                VmValue::Undefined => {}
                locale => {
                    let locale = to_string(heap, &locale)?;
                    ensure!(
                        locale.starts_with("en") || locale.starts_with("zh"),
                        "RangeError: locale {locale} is not available in the flow runtime"
                    );
                }
            }
            VmValue::String(locale_string(value))
        }
        "valueOf" => number(value),
        _ => bail!("TypeError: number.{name} is not a function"),
    })
}

fn regex_method(heap: &mut Heap, pattern: &str, flags: &str, name: &str, args: &[VmValue]) -> Result<VmValue> {
    let re = super::values::compile_regex(pattern, flags)?;
    let input = to_string(heap, &arg(args, 0))?;
    ensure!(input.len() <= 2 * 1024 * 1024, "RangeError: regex input exceeds 2 MiB");
    Ok(match name {
        "test" => VmValue::Boolean(re.is_match(&input)),
        "exec" => match re.captures(&input) {
            Some(captures) => match_array(heap, &captures)?,
            None => VmValue::Null,
        },
        "toString" => VmValue::String(format!("/{pattern}/{flags}")),
        _ => bail!("TypeError: regexp.{name} is not a function"),
    })
}

/// Call `receiver.name(...args)` for a built-in receiver kind.
pub(super) fn call_method(heap: &mut Heap, receiver: VmValue, name: &str, args: &[VmValue]) -> Result<VmValue> {
    match receiver {
        VmValue::String(text) => string_method(heap, &text, name, args),
        VmValue::Number(value) => number_method(heap, value.get(), name, args),
        VmValue::Boolean(value) => match name {
            "toString" => Ok(VmValue::String(value.to_string())),
            "valueOf" => Ok(VmValue::Boolean(value)),
            _ => bail!("TypeError: boolean.{name} is not a function"),
        },
        VmValue::Reference(id) => {
            if matches!(heap.get(id)?, HeapNode::Array(_)) {
                return array_method(heap, id, name, args);
            }
            if let Some((pattern, flags)) = heap.regex_parts(id) {
                return regex_method(heap, &pattern, &flags, name, args);
            }
            match name {
                "toString" => Ok(VmValue::String(to_string(heap, &receiver)?)),
                "hasOwnProperty" => {
                    let key = to_string(heap, &arg(args, 0))?;
                    Ok(VmValue::Boolean(match heap.get(id)? {
                        HeapNode::Object(properties) => properties.iter().any(|(k, _)| *k == key),
                        _ => false,
                    }))
                }
                _ => bail!("TypeError: object.{name} is not a function"),
            }
        }
        VmValue::Undefined | VmValue::Null => {
            bail!("TypeError: Cannot read properties of {} (reading '{name}')", if matches!(receiver, VmValue::Null) { "null" } else { "undefined" })
        }
        VmValue::Promise(_) => bail!("TypeError: promise.{name} is not available"),
    }
}

// ---------------------------------------------------------------- globals

fn parse_int(text: &str, radix: f64) -> f64 {
    let mut s = text.trim_start_matches(is_js_whitespace);
    let mut sign = 1.0;
    if let Some(rest) = s.strip_prefix('-') {
        sign = -1.0;
        s = rest;
    } else if let Some(rest) = s.strip_prefix('+') {
        s = rest;
    }
    let mut radix = radix as u32;
    let mut strip_prefix = true;
    if radix != 0 {
        if !(2..=36).contains(&radix) {
            return f64::NAN;
        }
        if radix != 16 {
            strip_prefix = false;
        }
    } else {
        radix = 10;
    }
    if strip_prefix && (s.starts_with("0x") || s.starts_with("0X")) {
        s = &s[2..];
        radix = 16;
    }
    let digits: String = s.chars().take_while(|c| c.to_digit(radix).is_some()).collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut value = 0.0f64;
    for c in digits.chars() {
        value = value * radix as f64 + c.to_digit(radix).unwrap() as f64;
    }
    sign * value
}

fn parse_float(text: &str) -> f64 {
    let s = text.trim_start_matches(is_js_whitespace);
    for prefix in ["Infinity", "+Infinity", "-Infinity"] {
        if s.starts_with(prefix) {
            return if prefix.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
        }
    }
    // The longest prefix that is a StrDecimalLiteral.
    let bytes = s.as_bytes();
    let mut best = None;
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i = 1;
    }
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if digits > 0 {
        best = Some(i);
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
        if digits > 0 {
            best = Some(i);
        }
    }
    if digits > 0 && i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        let mut j = i + 1;
        if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
            j += 1;
        }
        let start = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > start {
            best = Some(j);
        }
    }
    match best {
        Some(end) => string_to_number(&s[..end]),
        None => f64::NAN,
    }
}

/// Own enumerable string keys in OrdinaryOwnPropertyKeys order: array
/// indices ascending, then the rest in insertion order.
fn own_keys(heap: &Heap, value: &VmValue) -> Result<Vec<(String, VmValue)>> {
    Ok(match value {
        VmValue::Reference(id) => match heap.get(*id)? {
            HeapNode::Object(properties) => {
                let index = |key: &str| key.parse::<u32>().ok().filter(|n| *n != u32::MAX && n.to_string() == key);
                let mut ordered: Vec<_> = properties.iter().cloned().collect();
                ordered.sort_by_key(|(key, _)| {
                    let i = index(key);
                    (i.is_none(), i.unwrap_or(0))
                });
                ordered
            }
            HeapNode::Array(items) => {
                let holes = heap.holes(*id);
                items
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !holes.contains(i))
                    .map(|(i, v)| (i.to_string(), v.clone()))
                    .collect()
            }
            _ => Vec::new(),
        },
        VmValue::String(text) => text.chars().enumerate().map(|(i, c)| (i.to_string(), VmValue::String(c.to_string()))).collect(),
        VmValue::Undefined | VmValue::Null => bail!("TypeError: Cannot convert undefined or null to object"),
        _ => Vec::new(),
    })
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JSON.stringify with an optional key list replacer and indentation.
fn json_stringify(heap: &Heap, value: &VmValue, keys: Option<&[String]>, gap: &str) -> Result<Option<String>> {
    fn serialize(
        heap: &Heap,
        value: &VmValue,
        keys: Option<&[String]>,
        gap: &str,
        indent: &str,
        stack: &mut Vec<HeapId>,
    ) -> Result<Option<String>> {
        Ok(Some(match value {
            VmValue::Null => "null".into(),
            VmValue::Boolean(v) => v.to_string(),
            VmValue::Number(n) => {
                let n = n.get();
                if n.is_finite() { number_to_string(n) } else { "null".into() }
            }
            VmValue::String(s) => quote(s),
            VmValue::Undefined => return Ok(None),
            VmValue::Promise(_) => "{}".into(),
            VmValue::Reference(id) => {
                let node = heap.get(*id)?;
                if matches!(node, HeapNode::Closure { .. }) {
                    return Ok(None);
                }
                ensure!(!stack.contains(id), "TypeError: Converting circular structure to JSON");
                stack.push(*id);
                let inner = format!("{indent}{gap}");
                let result = match node {
                    HeapNode::Array(items) => {
                        let holes = heap.holes(*id);
                        let mut parts = Vec::with_capacity(items.len());
                        for (index, item) in items.iter().enumerate() {
                            let item = if holes.contains(&index) { &VmValue::Undefined } else { item };
                            parts.push(serialize(heap, item, keys, gap, &inner, stack)?.unwrap_or_else(|| "null".into()));
                        }
                        if parts.is_empty() {
                            "[]".into()
                        } else if gap.is_empty() {
                            format!("[{}]", parts.join(","))
                        } else {
                            format!("[\n{inner}{}\n{indent}]", parts.join(&format!(",\n{inner}")))
                        }
                    }
                    _ => {
                        let mut properties = own_keys(heap, value)?;
                        if let Some(keys) = keys {
                            properties = keys
                                .iter()
                                .filter_map(|key| properties.iter().find(|(k, _)| k == key).cloned())
                                .collect();
                        }
                        let mut parts = Vec::new();
                        for (key, item) in properties {
                            if let Some(text) = serialize(heap, &item, keys, gap, &inner, stack)? {
                                let separator = if gap.is_empty() { ":" } else { ": " };
                                parts.push(format!("{}{separator}{text}", quote(&key)));
                            }
                        }
                        if parts.is_empty() {
                            "{}".into()
                        } else if gap.is_empty() {
                            format!("{{{}}}", parts.join(","))
                        } else {
                            format!("{{\n{inner}{}\n{indent}}}", parts.join(&format!(",\n{inner}")))
                        }
                    }
                };
                stack.pop();
                result
            }
        }))
    }
    let text = serialize(heap, value, keys, gap, "", &mut Vec::new())?;
    if let Some(text) = &text {
        ensure!(text.len() <= TEXT_RESULT_LIMIT, "RangeError: JSON.stringify result exceeds 4 MiB");
    }
    Ok(text)
}

/// Global functions and static methods, by their dotted name.
pub(super) fn builtin_arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "Math.abs" | "Math.floor" | "Math.ceil" | "Math.round" | "Math.trunc" | "Math.sign"
        | "Math.sqrt" | "Math.cbrt" | "Math.exp" | "Math.log" | "Math.log2" | "Math.log10"
        | "Math.sin" | "Math.cos" | "Math.tan" | "Math.asin" | "Math.acos" | "Math.atan" => (1, 1),
        "Math.pow" | "Math.atan2" => (2, 2),
        "Math.max" | "Math.min" | "Math.hypot" => (0, 64),
        "Math.random" => (0, 0),
        "Number" | "String" | "Boolean" => (0, 1),
        "parseInt" | "Number.parseInt" => (1, 2),
        "parseFloat" | "Number.parseFloat" | "isNaN" | "isFinite" | "Number.isInteger"
        | "Number.isSafeInteger" | "Number.isFinite" | "Number.isNaN" => (1, 1),
        "Object.keys" | "Object.values" | "Object.entries" | "Object.fromEntries" | "Array.isArray"
        | "Array.from" => (1, 1),
        "Object.assign" => (1, 64),
        "Array.of" => (0, 64),
        "JSON.stringify" => (1, 3),
        "JSON.parse" => (1, 1),
        "delete" => (2, 2),
        _ if name.strip_prefix("new ").is_some_and(|class| super::vm_eval::ERROR_CLASSES.contains(&class)) => (0, 2),
        _ => return None,
    })
}

/// Static constants such as Math.PI, as source text for a numeric literal.
pub(super) fn constant(name: &str) -> Option<f64> {
    Some(match name {
        "Math.PI" => std::f64::consts::PI,
        "Math.E" => std::f64::consts::E,
        "Math.LN2" => std::f64::consts::LN_2,
        "Math.LN10" => std::f64::consts::LN_10,
        "Math.LOG2E" => std::f64::consts::LOG2_E,
        "Math.LOG10E" => std::f64::consts::LOG10_E,
        "Math.SQRT2" => std::f64::consts::SQRT_2,
        "Math.SQRT1_2" => std::f64::consts::FRAC_1_SQRT_2,
        "Number.MAX_SAFE_INTEGER" => 9007199254740991.0,
        "Number.MIN_SAFE_INTEGER" => -9007199254740991.0,
        "Number.EPSILON" => f64::EPSILON,
        "Number.MAX_VALUE" => f64::MAX,
        "Number.MIN_VALUE" => 5e-324,
        "Number.POSITIVE_INFINITY" | "Infinity" => f64::INFINITY,
        "Number.NEGATIVE_INFINITY" => f64::NEG_INFINITY,
        "Number.NaN" | "NaN" => f64::NAN,
        _ => return None,
    })
}

fn math(name: &str, values: &[f64]) -> f64 {
    let x = values.first().copied().unwrap_or(f64::NAN);
    match name {
        "Math.abs" => x.abs(),
        "Math.floor" => x.floor(),
        "Math.ceil" => x.ceil(),
        "Math.round" => {
            if !x.is_finite() || x == 0.0 {
                x
            } else {
                let floor = x.floor();
                if x - floor >= 0.5 { floor + 1.0 } else { floor }
            }
        }
        "Math.trunc" => x.trunc(),
        "Math.sign" => {
            if x.is_nan() || x == 0.0 { x } else { x.signum() }
        }
        "Math.sqrt" => x.sqrt(),
        "Math.cbrt" => x.cbrt(),
        "Math.exp" => x.exp(),
        "Math.log" => x.ln(),
        "Math.log2" => x.log2(),
        "Math.log10" => x.log10(),
        "Math.sin" => x.sin(),
        "Math.cos" => x.cos(),
        "Math.tan" => x.tan(),
        "Math.asin" => x.asin(),
        "Math.acos" => x.acos(),
        "Math.atan" => x.atan(),
        "Math.atan2" => x.atan2(values.get(1).copied().unwrap_or(f64::NAN)),
        "Math.pow" => js_pow(x, values.get(1).copied().unwrap_or(f64::NAN)),
        "Math.max" => values.iter().fold(f64::NEG_INFINITY, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.max(*b) }),
        "Math.min" => values.iter().fold(f64::INFINITY, |a, b| if a.is_nan() || b.is_nan() { f64::NAN } else { a.min(*b) }),
        "Math.hypot" => values.iter().map(|v| v * v).sum::<f64>().sqrt(),
        _ => f64::NAN,
    }
}

/// Number::exponentiate, which differs from powf for |base| = 1 and an
/// infinite or NaN exponent.
pub(super) fn js_pow(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() || (base.abs() == 1.0 && exponent.is_infinite()) {
        f64::NAN
    } else {
        base.powf(exponent)
    }
}

pub(super) fn builtin(heap: &mut Heap, name: &str, args: &[VmValue]) -> Result<VmValue> {
    let first = arg(args, 0);
    if let Some(class) = name.strip_prefix("new ") {
        ensure!(super::vm_eval::ERROR_CLASSES.contains(&class), "TypeError: {class} is not a constructor");
        return new_error(heap, class, &first, &arg(args, 1));
    }
    Ok(match name {
        "$sortWriteBack" => {
            // sort() in place: the sorted values, then the original holes.
            let (VmValue::Reference(target), VmValue::Reference(sorted)) = (&first, &arg(args, 1)) else {
                bail!("internal sort write-back needs arrays")
            };
            let original = array_items(heap, *target)?.len();
            let mut items = array_items(heap, *sorted)?;
            let count = items.len();
            items.resize(original.max(count), VmValue::Undefined);
            heap.replace_array(*target, items, (count..original).collect())?;
            VmValue::Reference(*target)
        }
        "delete" => {
            if let VmValue::Reference(id) = first {
                let key = to_string(heap, &arg(args, 1))?;
                heap.delete_property(id, &key)?;
            }
            VmValue::Boolean(true)
        }
        _ if name.starts_with("Math.") => {
            if name == "Math.random" {
                return Ok(number(rand_unit()));
            }
            let mut values = Vec::with_capacity(args.len());
            for value in args {
                values.push(to_number(heap, value)?);
            }
            number(math(name, &values))
        }
        "Number" => number(if args.is_empty() { 0.0 } else { to_number(heap, &first)? }),
        "String" => VmValue::String(if args.is_empty() { String::new() } else { to_string(heap, &first)? }),
        "Boolean" => VmValue::Boolean(super::vm_eval::truthy(&first)),
        "parseInt" | "Number.parseInt" => {
            let text = to_string(heap, &first)?;
            let radix = match arg(args, 1) {
                VmValue::Undefined => 0.0,
                value => {
                    let n = to_number(heap, &value)?;
                    if n.is_finite() { n.trunc() } else { 0.0 }
                }
            };
            number(parse_int(&text, radix))
        }
        "parseFloat" | "Number.parseFloat" => number(parse_float(&to_string(heap, &first)?)),
        "isNaN" => VmValue::Boolean(to_number(heap, &first)?.is_nan()),
        "isFinite" => VmValue::Boolean(to_number(heap, &first)?.is_finite()),
        "Number.isNaN" => VmValue::Boolean(matches!(first, VmValue::Number(n) if n.get().is_nan())),
        "Number.isFinite" => VmValue::Boolean(matches!(first, VmValue::Number(n) if n.get().is_finite())),
        "Number.isInteger" => VmValue::Boolean(matches!(first, VmValue::Number(n) if n.get().is_finite() && n.get().trunc() == n.get())),
        "Number.isSafeInteger" => VmValue::Boolean(
            matches!(first, VmValue::Number(n) if n.get().is_finite() && n.get().trunc() == n.get() && n.get().abs() <= 9007199254740991.0),
        ),
        "Array.isArray" => VmValue::Boolean(matches!(&first, VmValue::Reference(id) if matches!(heap.get(*id), Ok(HeapNode::Array(_))))),
        "Array.of" => new_array(heap, args.to_vec())?,
        "Array.from" => match &first {
            VmValue::String(text) => new_array(heap, text.chars().map(|c| VmValue::String(c.to_string())).collect())?,
            VmValue::Reference(id) => match heap.get(*id)? {
                HeapNode::Array(items) => {
                    let holes = heap.holes(*id);
                    let items = items.iter().enumerate().map(|(i, v)| if holes.contains(&i) { VmValue::Undefined } else { v.clone() }).collect();
                    new_array(heap, items)?
                }
                HeapNode::Object(_) => {
                    // An array-like {length: n} produces n elements.
                    let length = to_number(heap, &heap.property(*id, "length")?)?;
                    let length = if length.is_nan() { 0.0 } else { integer_or_infinity(length).max(0.0) };
                    ensure!(length <= ARRAY_LIMIT as f64, "RangeError: Invalid array length");
                    let mut items = Vec::with_capacity(length as usize);
                    for index in 0..length as usize {
                        items.push(heap.property(*id, &index.to_string())?);
                    }
                    new_array(heap, items)?
                }
                _ => new_array(heap, Vec::new())?,
            },
            VmValue::Undefined | VmValue::Null => bail!("TypeError: Array.from requires an array-like object"),
            _ => new_array(heap, Vec::new())?,
        },
        "Object.keys" | "Object.values" | "Object.entries" => {
            let properties = own_keys(heap, &first)?;
            let mut out = Vec::with_capacity(properties.len());
            for (key, value) in properties {
                out.push(match name {
                    "Object.keys" => VmValue::String(key),
                    "Object.values" => value,
                    _ => new_array(heap, vec![VmValue::String(key), value])?,
                });
            }
            new_array(heap, out)?
        }
        "Object.fromEntries" => {
            let VmValue::Reference(id) = first else {
                bail!("TypeError: Object.fromEntries requires an array of entries")
            };
            let entries = array_items(heap, id)?;
            let mut properties: Vec<(String, VmValue)> = Vec::new();
            for entry in entries {
                let VmValue::Reference(pair) = entry else {
                    bail!("TypeError: Iterator value is not an entry object")
                };
                let pair = array_items(heap, pair)?;
                let key = to_string(heap, pair.first().unwrap_or(&VmValue::Undefined))?;
                let value = pair.get(1).cloned().unwrap_or(VmValue::Undefined);
                if let Some(existing) = properties.iter_mut().find(|(k, _)| *k == key) {
                    existing.1 = value;
                } else {
                    properties.push((key, value));
                }
            }
            VmValue::Reference(heap.allocate(HeapNode::Object(properties))?)
        }
        "Object.assign" => {
            let VmValue::Reference(target) = first else {
                bail!("TypeError: Object.assign target must be an object")
            };
            for source in &args[1..] {
                if matches!(source, VmValue::Undefined | VmValue::Null) {
                    continue;
                }
                for (key, value) in own_keys(heap, source)? {
                    heap.set_property(target, key, value)?;
                }
            }
            VmValue::Reference(target)
        }
        "JSON.stringify" => {
            let keys = match arg(args, 1) {
                VmValue::Reference(id) if matches!(heap.get(id)?, HeapNode::Array(_)) => {
                    let mut keys = Vec::new();
                    for key in array_items(heap, id)? {
                        if matches!(key, VmValue::String(_) | VmValue::Number(_)) {
                            let key = to_string(heap, &key)?;
                            if !keys.contains(&key) {
                                keys.push(key);
                            }
                        }
                    }
                    Some(keys)
                }
                VmValue::Reference(id) if matches!(heap.get(id)?, HeapNode::Closure { .. }) => {
                    bail!("TypeError: JSON.stringify replacer functions are not available yet")
                }
                _ => None,
            };
            let gap = match arg(args, 2) {
                VmValue::Number(n) => " ".repeat(integer_or_infinity(n.get()).clamp(0.0, 10.0) as usize),
                VmValue::String(s) => s.chars().take(10).collect(),
                _ => String::new(),
            };
            match json_stringify(heap, &first, keys.as_deref(), &gap)? {
                Some(text) => VmValue::String(text),
                None => VmValue::Undefined,
            }
        }
        "JSON.parse" => {
            let text = to_string(heap, &first)?;
            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("SyntaxError: {e}"))?;
            heap.from_json(&parsed)?
        }
        _ => {
            // path.* and url.* keep their JSON implementation.
            let mut values = Vec::with_capacity(args.len());
            for value in args {
                values.push(heap.to_json(value)?);
            }
            // The built-ins name themselves in their errors.
            let result = super::builtins::call(name, values)?;
            heap.from_json(&result)?
        }
    })
}

/// `new Error(message, {cause})`: name and message are stored as properties
/// so that they also reach AI tasks and tools across the JSON boundary.
pub(super) fn new_error(heap: &mut Heap, class: &str, message: &VmValue, options: &VmValue) -> Result<VmValue> {
    let message = match message {
        VmValue::Undefined => String::new(),
        value => to_string(heap, value)?,
    };
    let mut properties = vec![
        ("name".to_owned(), VmValue::String(class.to_owned())),
        ("message".to_owned(), VmValue::String(message)),
    ];
    if let VmValue::Reference(id) = options
        && matches!(heap.get(*id)?, HeapNode::Object(_))
    {
        let cause = heap.property(*id, "cause")?;
        if !matches!(cause, VmValue::Undefined) {
            properties.push(("cause".to_owned(), cause));
        }
    }
    let id = heap.allocate(HeapNode::Object(properties))?;
    heap.set_class(id, class);
    Ok(VmValue::Reference(id))
}

/// Math.random from the operating system's randomness; flow state is saved
/// after each step, so the value never needs to be reproduced.
fn rand_unit() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        bytes = hasher.finish().to_le_bytes();
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_rounds_the_exact_binary_value_half_up() {
        // Expected strings come from Node.js 24.
        for (value, digits, expected) in [
            (1.005, 2, "1.00"),
            (2.5, 0, "3"),
            (0.5, 0, "1"),
            (-1.5, 0, "-2"),
            (1234.5678, 1, "1234.6"),
            (1e21, 2, "1e+21"),
            (0.000001, 7, "0.0000010"),
            (-0.0001, 2, "0.00"),
        ] {
            assert_eq!(to_fixed(value, digits), expected, "{value}.toFixed({digits})");
        }
    }

    #[test]
    fn substitution_patterns_match_javascript() {
        let text = "aXbXc";
        assert_eq!(substitution("[$&]", text, 1, 2, &[], &[]), "[X]");
        assert_eq!(substitution("$$", text, 1, 2, &[], &[]), "$");
        assert_eq!(substitution("$1", "x", 0, 1, &[], &[]), "$1");
        assert_eq!(substitution("<$1>", "a1", 1, 2, &[Some("1".into())], &[]), "<1>");
        assert_eq!(substitution("$`|$'", text, 1, 2, &[], &[]), "a|bXc");
    }

    #[test]
    fn number_parsing_globals_match_javascript() {
        assert_eq!(parse_int("  42px", 0.0), 42.0);
        assert_eq!(parse_int("0x1F", 0.0), 31.0);
        assert_eq!(parse_int("1F", 16.0), 31.0);
        assert_eq!(parse_int("z", 36.0), 35.0);
        assert!(parse_int("", 0.0).is_nan());
        assert_eq!(parse_int("-12.9", 0.0), -12.0);
        assert_eq!(parse_float("3.14abc"), 3.14);
        assert_eq!(parse_float(".5e2x"), 50.0);
        assert_eq!(parse_float("Infinityx"), f64::INFINITY);
        assert_eq!(locale_string(1234567.891), "1,234,567.891");
        assert_eq!(locale_string(0.1), "0.1");
        assert_eq!(radix_string(255.0, 16), "ff");
        assert_eq!(radix_string(255.0, 2), "11111111");
    }
}
