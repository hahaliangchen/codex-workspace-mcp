//! Deterministic JSON operations. No model fallback and no JavaScript runtime.
use super::*;
use anyhow::{Context, Result, bail, ensure};

fn integer(v: &Value) -> Option<i128> {
    v.as_i64()
        .map(i128::from)
        .or_else(|| v.as_u64().map(i128::from))
}
fn number(v: &Value) -> Result<f64> {
    if let Some(i) = integer(v) {
        ensure!(
            i.abs() <= 9_007_199_254_740_991,
            "integer cannot be converted to float without loss"
        );
    }
    let n = v.as_f64().context("numeric operand required")?;
    ensure!(n.is_finite(), "finite numeric operand required");
    Ok(n)
}
fn int_value(n: i128) -> Result<Value> {
    if let Ok(n) = i64::try_from(n) {
        return Ok(json!(n));
    }
    if let Ok(n) = u64::try_from(n) {
        return Ok(json!(n));
    }
    bail!("integer overflow")
}
fn float_value(n: f64) -> Result<Value> {
    ensure!(n.is_finite(), "non-finite arithmetic result");
    Ok(number_value(n))
}
pub(super) fn boolean(v: &Value) -> Result<bool> {
    Ok(match v {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().context("invalid JSON number")? != 0.0,
        Value::String(_) | Value::Array(_) | Value::Object(_) => true,
    })
}
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
fn equal(a: &Value, b: &Value) -> Result<bool> {
    Ok(match (a, b) {
        (Value::Number(_), Value::Number(_)) => match (integer(a), integer(b)) {
            (Some(a), Some(b)) => a == b,
            _ => number(a)? == number(b)?,
        },
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut same = true;
                for (a, b) in a.iter().zip(b) {
                    if !equal(a, b)? {
                        same = false;
                        break;
                    }
                }
                same
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            if a.len() != b.len() {
                false
            } else {
                let mut same = true;
                for (k, a) in a {
                    if let Some(b) = b.get(k) {
                        if !equal(a, b)? {
                            same = false;
                            break;
                        }
                    } else {
                        same = false;
                        break;
                    }
                }
                same
            }
        }
        _ => a == b,
    })
}
pub(super) fn binary(op: &str, a: Value, b: Value) -> Result<Value> {
    if matches!(op, "==" | "===" | "!=" | "!==") {
        let same = equal(&a, &b)?;
        return Ok(json!(if matches!(op, "!=" | "!==") {
            !same
        } else {
            same
        }));
    }
    if matches!(op, "<" | "<=" | ">" | ">=") {
        let ordering = match (&a, &b) {
            (Value::String(a), Value::String(b)) => a.cmp(b),
            (Value::Number(_), Value::Number(_)) => match (integer(&a), integer(&b)) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => number(&a)?
                    .partial_cmp(&number(&b)?)
                    .context("invalid numeric comparison")?,
            },
            _ => bail!("comparison needs two numbers or two strings"),
        };
        return Ok(json!(match op {
            "<" => ordering.is_lt(),
            "<=" => !ordering.is_gt(),
            ">" => ordering.is_gt(),
            _ => !ordering.is_lt(),
        }));
    }
    // A string operand selects concatenation, as in ordinary flow source.
    // Convert JSON primitives directly, preserving 64-bit integer text rather
    // than passing through f64. Containers still require JSON.stringify.
    if op == "+" && (a.is_string() || b.is_string()) {
        return Ok(json!(format!("{}{}", text(&a)?, text(&b)?)));
    }
    if op != "/"
        && let (Some(a), Some(b)) = (integer(&a), integer(&b))
    {
        let n = match op {
            "+" => a.checked_add(b),
            "-" => a.checked_sub(b),
            "*" => a.checked_mul(b),
            "%" => {
                ensure!(b != 0, "remainder by zero");
                a.checked_rem(b)
            }
            _ => bail!("unsupported binary operator {op}"),
        }
        .context("integer overflow")?;
        return int_value(n);
    }
    let (a, b) = (number(&a)?, number(&b)?);
    float_value(match op {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => {
            ensure!(b != 0.0, "division by zero");
            a / b
        }
        "%" => {
            ensure!(b != 0.0, "remainder by zero");
            a % b
        }
        _ => bail!("unsupported binary operator {op}"),
    })
}
pub(super) fn unary(op: &str, v: Value) -> Result<Value> {
    match op {
        "!" => Ok(json!(!boolean(&v)?)),
        "bool" => Ok(json!(boolean(&v)?)),
        "+" => {
            ensure!(v.is_number(), "numeric operand required");
            Ok(v)
        }
        "-" => {
            if let Some(i) = integer(&v) {
                int_value(-i)
            } else {
                float_value(-number(&v)?)
            }
        }
        _ => bail!("unsupported unary operator {op}"),
    }
}
pub(super) fn method_arity(name: &str) -> Option<(usize, usize)> {
    Some(match name {
        "includes" | "indexOf" | "startsWith" | "endsWith" | "split" => (1, 1),
        "slice" => (1, 2),
        "trim" | "toLowerCase" | "toUpperCase" => (0, 0),
        "join" => (0, 1),
        _ => return None,
    })
}
fn index(v: &Value, len: usize) -> Result<usize> {
    let n = integer(v).context("slice index must be an integer")?;
    let len = len as i128;
    Ok(if n < 0 { (len + n).max(0) } else { n.min(len) } as usize)
}
pub(super) fn method(receiver: Value, name: &str, args: Vec<Value>) -> Result<Value> {
    let (min, max) = method_arity(name).context("unsupported data method")?;
    ensure!(
        (min..=max).contains(&args.len()),
        "invalid method argument count"
    );
    if name == "slice" {
        let len = match &receiver {
            Value::String(s) => s.chars().count(),
            Value::Array(a) => a.len(),
            _ => bail!("slice needs a string or array"),
        };
        let start = index(&args[0], len)?;
        let end = if args.len() == 2 {
            index(&args[1], len)?
        } else {
            len
        };
        return Ok(match receiver {
            Value::String(s) => json!(
                s.chars()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .collect::<String>()
            ),
            Value::Array(a) => json!(
                a.into_iter()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .collect::<Vec<_>>()
            ),
            _ => unreachable!(),
        });
    }
    match receiver {
        Value::String(s) => {
            if matches!(name, "trim" | "toLowerCase" | "toUpperCase") {
                return Ok(json!(match name {
                    "trim" => s.trim().to_owned(),
                    "toLowerCase" => s.to_lowercase(),
                    _ => s.to_uppercase(),
                }));
            }
            let arg = args
                .first()
                .and_then(Value::as_str)
                .context("string method needs a string argument")?;
            Ok(match name {
                "includes" => json!(s.contains(arg)),
                "indexOf" => json!(
                    s.find(arg)
                        .map(|byte| s[..byte].chars().count() as i64)
                        .unwrap_or(-1)
                ),
                "startsWith" => json!(s.starts_with(arg)),
                "endsWith" => json!(s.ends_with(arg)),
                "split" => {
                    if arg.is_empty() {
                        json!(s.chars().map(|c| c.to_string()).collect::<Vec<_>>())
                    } else {
                        json!(s.split(arg).collect::<Vec<_>>())
                    }
                }
                _ => bail!("method {name} is not supported on strings"),
            })
        }
        Value::Array(a) => match name {
            "includes" | "indexOf" => {
                let mut found = None;
                for (i, v) in a.iter().enumerate() {
                    if equal(v, &args[0])? {
                        found = Some(i);
                        break;
                    }
                }
                Ok(if name == "includes" {
                    json!(found.is_some())
                } else {
                    json!(found.map(|i| i as i64).unwrap_or(-1))
                })
            }
            "join" => {
                let separator = if let Some(v) = args.first() {
                    v.as_str().context("join separator must be a string")?
                } else {
                    ","
                };
                Ok(json!(
                    a.iter()
                        .map(text)
                        .collect::<Result<Vec<_>>>()?
                        .join(separator)
                ))
            }
            _ => bail!("method {name} is not supported on arrays"),
        },
        _ => bail!("data methods need a string or array receiver"),
    }
}
