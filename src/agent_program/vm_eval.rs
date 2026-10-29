//! Expression evaluation over identity-bearing VM values, following the
//! ECMAScript operator and property-access semantics.
use super::{
    Expr,
    heap::{Heap, HeapId, HeapNode, JsNumber, VmValue},
    js_methods, js_value as js,
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;

/// Compiler temporaries live in the frame; source variables live in the
/// frame's lexical environment.
pub(super) fn is_temp(name: &str) -> bool {
    name.starts_with("$temp")
}

/// What an expression can read: the frame's temporaries and its scope chain.
#[derive(Clone, Copy)]
pub(super) struct Locals<'a> {
    pub temps: &'a BTreeMap<String, VmValue>,
    pub env: Option<HeapId>,
}

pub(super) fn truthy(value: &VmValue) -> bool {
    match value {
        VmValue::Undefined | VmValue::Null => false,
        VmValue::Boolean(value) => *value,
        VmValue::Number(value) => value.get() != 0.0 && !value.get().is_nan(),
        VmValue::String(value) => !value.is_empty(),
        VmValue::Reference(_) | VmValue::Promise(_) => true,
    }
}

fn number(value: f64) -> VmValue {
    VmValue::Number(JsNumber::new(value))
}

/// ToInt32 and ToUint32.
fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() {
        0
    } else {
        number.trunc().rem_euclid(4294967296.0) as u32
    }
}
fn to_int32(number: f64) -> i32 {
    to_uint32(number) as i32
}

/// A canonical array index: "1" is an index, "01" and "1.0" are not.
fn canonical_index(key: &str) -> Option<usize> {
    let index: u32 = key.parse().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index as usize)
}

pub(super) const ERROR_CLASSES: [&str; 8] = [
    "Error",
    "TypeError",
    "RangeError",
    "SyntaxError",
    "ReferenceError",
    "EvalError",
    "URIError",
    "AggregateError",
];

/// The [[Get]] of a property, for each value kind of this runtime.
pub(super) fn member(heap: &Heap, object: &VmValue, key: &VmValue) -> Result<VmValue> {
    let name = js::to_string(heap, key)?;
    Ok(match object {
        VmValue::Undefined | VmValue::Null => bail!(
            "TypeError: Cannot read properties of {} (reading '{name}')",
            if matches!(object, VmValue::Null) { "null" } else { "undefined" }
        ),
        VmValue::Reference(id) => match heap.get(*id)? {
            HeapNode::Object(properties) => {
                if let Some((pattern, flags)) = heap.regex_parts(*id) {
                    return Ok(match name.as_str() {
                        "source" => VmValue::String(pattern),
                        "flags" => VmValue::String(flags),
                        "global" => VmValue::Boolean(flags.contains('g')),
                        "ignoreCase" => VmValue::Boolean(flags.contains('i')),
                        "multiline" => VmValue::Boolean(flags.contains('m')),
                        "dotAll" => VmValue::Boolean(flags.contains('s')),
                        "unicode" => VmValue::Boolean(flags.contains('u')),
                        "lastIndex" => number(0.0),
                        _ => VmValue::Undefined,
                    });
                }
                properties
                    .iter()
                    .find(|(property, _)| *property == name)
                    .map(|(_, value)| value.clone())
                    .unwrap_or(VmValue::Undefined)
            }
            HeapNode::Array(items) => {
                if name == "length" {
                    number(items.len() as f64)
                } else if let Some(index) = canonical_index(&name) {
                    if heap.has_array_index(*id, index)? {
                        items[index].clone()
                    } else {
                        VmValue::Undefined
                    }
                } else {
                    VmValue::Undefined
                }
            }
            HeapNode::Closure { function: id, .. } | HeapNode::Native(id) => match name.as_str() {
                "name" => VmValue::String(js::function_name(id).to_owned()),
                _ => VmValue::Undefined,
            },
            HeapNode::PromiseResolver { .. } => match name.as_str() {
                "name" => VmValue::String(String::new()),
                "length" => number(1.0),
                _ => VmValue::Undefined,
            },
            HeapNode::PromiseCapability { .. } => VmValue::Undefined,
            HeapNode::Promise { .. } => bail!("TypeError: this is a Promise; the value may need await"),
            HeapNode::Environment { .. } => VmValue::Undefined,
        },
        VmValue::String(text) => {
            if name == "length" {
                number(text.chars().count() as f64)
            } else if let Some(index) = canonical_index(&name) {
                text.chars()
                    .nth(index)
                    .map(|c| VmValue::String(c.to_string()))
                    .unwrap_or(VmValue::Undefined)
            } else {
                VmValue::Undefined
            }
        }
        VmValue::Boolean(_) | VmValue::Number(_) | VmValue::Promise(_) => VmValue::Undefined,
    })
}

fn instance_of(heap: &Heap, value: &VmValue, class: &str) -> Result<bool> {
    Ok(match value {
        VmValue::Reference(id) => match (heap.get(*id)?, class) {
            (HeapNode::Array(_), "Array" | "Object") => true,
            (HeapNode::Promise { .. }, "Promise" | "Object") => true,
            (HeapNode::Closure { .. } | HeapNode::Native(_) | HeapNode::PromiseResolver { .. }, "Function" | "Object") => true,
            (HeapNode::Object(_), "Object") => true,
            (HeapNode::Object(_), "RegExp") => heap.regex_parts(*id).is_some(),
            (HeapNode::Object(_), "Error") => heap
                .class_of(*id)
                .is_some_and(|actual| ERROR_CLASSES.contains(&actual)),
            (HeapNode::Object(_), class) => heap.class_of(*id) == Some(class),
            _ => false,
        },
        VmValue::Promise(_) => matches!(class, "Promise" | "Object"),
        _ => false,
    })
}

fn binary(heap: &Heap, op: &str, left: &VmValue, right: &VmValue) -> Result<VmValue> {
    Ok(match op {
        "===" => VmValue::Boolean(js::strict_equal(left, right)),
        "!==" => VmValue::Boolean(!js::strict_equal(left, right)),
        "==" => VmValue::Boolean(js::loose_equal(heap, left, right)?),
        "!=" => VmValue::Boolean(!js::loose_equal(heap, left, right)?),
        "+" => {
            let a = js::to_primitive(heap, left)?;
            let b = js::to_primitive(heap, right)?;
            if matches!(a, VmValue::String(_)) || matches!(b, VmValue::String(_)) {
                let text = format!("{}{}", js::to_string(heap, &a)?, js::to_string(heap, &b)?);
                ensure!(text.len() <= 4 * 1024 * 1024, "RangeError: Invalid string length");
                VmValue::String(text)
            } else {
                number(js::to_number(heap, &a)? + js::to_number(heap, &b)?)
            }
        }
        "<" | "<=" | ">" | ">=" => {
            let a = js::to_primitive(heap, left)?;
            let b = js::to_primitive(heap, right)?;
            if let (VmValue::String(a), VmValue::String(b)) = (&a, &b) {
                let a: Vec<u16> = a.encode_utf16().collect();
                let b: Vec<u16> = b.encode_utf16().collect();
                VmValue::Boolean(match op {
                    "<" => a < b,
                    "<=" => a <= b,
                    ">" => a > b,
                    _ => a >= b,
                })
            } else {
                let (a, b) = (js::to_number(heap, &a)?, js::to_number(heap, &b)?);
                VmValue::Boolean(match op {
                    "<" => a < b,
                    "<=" => a <= b,
                    ">" => a > b,
                    _ => a >= b,
                })
            }
        }
        "-" | "*" | "/" | "%" | "**" => {
            let (a, b) = (js::to_number(heap, left)?, js::to_number(heap, right)?);
            number(match op {
                "-" => a - b,
                "*" => a * b,
                "/" => a / b,
                "%" => a % b,
                _ => js_methods::js_pow(a, b),
            })
        }
        "&" | "|" | "^" | "<<" | ">>" | ">>>" => {
            let (a, b) = (js::to_number(heap, left)?, js::to_number(heap, right)?);
            let shift = to_uint32(b) & 31;
            number(match op {
                "&" => (to_int32(a) & to_int32(b)) as f64,
                "|" => (to_int32(a) | to_int32(b)) as f64,
                "^" => (to_int32(a) ^ to_int32(b)) as f64,
                "<<" => to_int32(a).wrapping_shl(shift) as f64,
                ">>" => (to_int32(a) >> shift) as f64,
                _ => (to_uint32(a) >> shift) as f64,
            })
        }
        "in" => {
            let key = js::to_string(heap, left)?;
            match right {
                VmValue::Reference(id) => VmValue::Boolean(match heap.get(*id)? {
                    HeapNode::Object(properties) => properties.iter().any(|(k, _)| *k == key),
                    HeapNode::Array(items) => {
                        key == "length"
                            || canonical_index(&key)
                                .is_some_and(|i| i < items.len() && heap.has_array_index(*id, i).unwrap_or(false))
                    }
                    _ => false,
                }),
                _ => bail!("TypeError: Cannot use 'in' operator to search for '{key}' in a non-object"),
            }
        }
        "instanceof" => {
            let VmValue::String(class) = right else {
                bail!("TypeError: Right-hand side of 'instanceof' is not callable")
            };
            VmValue::Boolean(instance_of(heap, left, class)?)
        }
        _ => bail!("unsupported binary operator {op}"),
    })
}

pub(super) fn eval(expr: &Expr, locals: Locals<'_>, heap: &mut Heap) -> Result<VmValue> {
    Ok(match expr {
        Expr::Undefined => VmValue::Undefined,
        Expr::Value(value) => heap.from_json(value)?,
        Expr::Variable(name) if is_temp(name) => locals
            .temps
            .get(name)
            .with_context(|| format!("ReferenceError: {name} is not defined"))?
            .clone(),
        Expr::Variable(name) => match locals.env {
            Some(env) => heap.lookup_binding(env, name)?,
            None => bail!("ReferenceError: {name} is not defined"),
        },
        Expr::Array(items) => {
            let values = items
                .iter()
                .map(|item| eval(item, locals, heap))
                .collect::<Result<Vec<_>>>()?;
            VmValue::Reference(heap.allocate(HeapNode::Array(values))?)
        }
        Expr::Object(fields) => {
            let mut result = Vec::new();
            for (key, value) in fields {
                let value = eval(value, locals, heap)?;
                if let Some((_, existing)) = result.iter_mut().find(|(name, _)| name == key) {
                    *existing = value;
                } else {
                    result.push((key.clone(), value));
                }
            }
            VmValue::Reference(heap.allocate(HeapNode::Object(result))?)
        }
        Expr::Member(object, key) => {
            let object = eval(object, locals, heap)?;
            let key = eval(key, locals, heap)?;
            member(heap, &object, &key)?
        }
        Expr::Binary(op, left, right) => {
            let left = eval(left, locals, heap)?;
            if op == "hasIndex" {
                let VmValue::Reference(array) = left else {
                    bail!("array iteration requires an array")
                };
                let index = eval(right, locals, heap)?;
                let VmValue::Number(index) = index else {
                    bail!("invalid array index")
                };
                let index = index.get();
                if !index.is_finite() || index < 0.0 || index.fract() != 0.0 {
                    bail!("invalid array index")
                }
                VmValue::Boolean(heap.has_array_index(array, index as usize)?)
            } else if op == "??" && matches!(left, VmValue::Null | VmValue::Undefined) {
                eval(right, locals, heap)?
            } else if op == "??" {
                left
            } else if (op == "&&" && !truthy(&left)) || (op == "||" && truthy(&left)) {
                left
            } else {
                let right = eval(right, locals, heap)?;
                if op == "&&" || op == "||" {
                    right
                } else {
                    binary(heap, op, &left, &right)?
                }
            }
        }
        Expr::Unary(op, value) => {
            let value = eval(value, locals, heap)?;
            match op.as_str() {
                "!" => VmValue::Boolean(!truthy(&value)),
                "bool" => VmValue::Boolean(truthy(&value)),
                "+" => number(js::to_number(heap, &value)?),
                "-" => number(-js::to_number(heap, &value)?),
                "~" => number(!to_int32(js::to_number(heap, &value)?) as f64),
                "void" => VmValue::Undefined,
                "typeof" => VmValue::String(
                    match &value {
                        VmValue::Undefined => "undefined",
                        VmValue::Null => "object",
                        VmValue::Boolean(_) => "boolean",
                        VmValue::Number(_) => "number",
                        VmValue::String(_) => "string",
                        VmValue::Promise(_) => "object",
                        VmValue::Reference(id) => {
                            if matches!(heap.get(*id)?, HeapNode::Closure { .. } | HeapNode::Native(_) | HeapNode::PromiseResolver { .. }) {
                                "function"
                            } else {
                                "object"
                            }
                        }
                    }
                    .into(),
                ),
                "isArray" => js_methods::builtin(heap, "Array.isArray", &[value])?,
                "arrayLength" => match value {
                    VmValue::Reference(id) => match heap.get(id)? {
                        HeapNode::Array(items) => number(items.len() as f64),
                        _ => bail!("TypeError: array method receiver must be an array"),
                    },
                    _ => bail!("TypeError: array method receiver must be an array"),
                },
                "objectKeys" => js_methods::builtin(heap, "Object.keys", &[value])?,
                "objectValues" => js_methods::builtin(heap, "Object.values", &[value])?,
                _ => bail!("unsupported VM unary operator {op}"),
            }
        }
        Expr::Conditional(test, yes, no) => {
            let value = eval(test, locals, heap)?;
            if truthy(&value) {
                eval(yes, locals, heap)?
            } else {
                eval(no, locals, heap)?
            }
        }
        Expr::Stringify(value) => {
            let value = eval(value, locals, heap)?;
            js_methods::builtin(heap, "JSON.stringify", &[value])?
        }
        Expr::ParseJson(value) => {
            let value = eval(value, locals, heap)?;
            js_methods::builtin(heap, "JSON.parse", &[value])?
        }
        Expr::Template(parts) => {
            let mut text = String::new();
            for part in parts {
                let value = eval(part, locals, heap)?;
                text.push_str(&js::to_string(heap, &value)?);
                ensure!(text.len() <= 4 * 1024 * 1024, "RangeError: Invalid string length");
            }
            VmValue::String(text)
        }
        Expr::StringMethod(receiver, name, args) => {
            let value = eval(receiver, locals, heap)?;
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(arg, locals, heap)?);
            }
            js_methods::call_method(heap, value, name, &values)?
        }
        Expr::NodeBuiltin(name, args) => {
            let mut values = Vec::with_capacity(args.len());
            for arg in args {
                values.push(eval(arg, locals, heap)?);
            }
            js_methods::builtin(heap, name, &values)?
        }
        Expr::RegExp(args) => {
            let mut values = Vec::new();
            for arg in args {
                let value = eval(arg, locals, heap)?;
                values.push(heap.to_json(&value)?);
            }
            let result = super::values::regexp(values)?;
            heap.from_json(&result)?
        }
        // A closure captures the scope it is created in.
        Expr::Function(id) => {
            let environment = locals.env.context("a function needs an enclosing scope")?;
            VmValue::Reference(heap.allocate(HeapNode::Closure {
                function: id.clone(),
                environment,
            })?)
        }
        Expr::Native(name) => VmValue::Reference(heap.native(name)?),
    })
}

/// Whether a value can be called.
pub(super) fn callable(heap: &Heap, value: &VmValue) -> bool {
    matches!(value, VmValue::Reference(id)
        if matches!(heap.get(*id), Ok(HeapNode::Closure { .. } | HeapNode::Native(_) | HeapNode::PromiseResolver { .. })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn primitives_and_references_use_vm_semantics_without_json_roundtrip() {
        let mut heap = Heap::default();
        let object = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let other = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        assert!(js::strict_equal(
            &VmValue::Reference(object),
            &VmValue::Reference(object)
        ));
        assert!(!js::strict_equal(
            &VmValue::Reference(object),
            &VmValue::Reference(other)
        ));
        assert!(!js::strict_equal(&number(f64::NAN), &number(f64::NAN)));
        assert!(js::strict_equal(&number(-0.0), &number(0.0)));
        assert!(!truthy(&VmValue::String(String::new())));
        assert!(truthy(&VmValue::Reference(object)));
        let heap = heap;
        assert_eq!(
            binary(&heap, "==", &VmValue::String("01".into()), &VmValue::String("1".into())).unwrap(),
            VmValue::Boolean(false)
        );
        assert_eq!(
            binary(&heap, "==", &VmValue::String("1".into()), &number(1.0)).unwrap(),
            VmValue::Boolean(true)
        );
        assert_eq!(
            binary(&heap, "/", &number(1.0), &number(0.0)).unwrap(),
            number(f64::INFINITY)
        );
    }

    #[test]
    fn object_members_preserve_identity_across_evaluation_and_restore() {
        let mut heap = Heap::default();
        let mut temps = BTreeMap::new();
        let value = eval(
            &Expr::Object(vec![("x".into(), Expr::Value(json!(1)))]),
            Locals { temps: &temps, env: None },
            &mut heap,
        )
        .unwrap();
        let env = heap
            .new_environment(None, [("a".to_owned(), value.clone())].into(), Default::default())
            .unwrap();
        temps.insert("$temp1".into(), value);
        let encoded = serde_json::to_vec(&(heap, temps)).unwrap();
        let (mut heap, temps): (Heap, BTreeMap<String, VmValue>) =
            serde_json::from_slice(&encoded).unwrap();
        let VmValue::Reference(object) = temps["$temp1"] else {
            panic!("expected reference")
        };
        heap.set_property(object, "x".into(), number(2.0)).unwrap();
        // The variable in the environment and the temporary are one object.
        let value = eval(
            &Expr::Member(
                Box::new(Expr::Variable("a".into())),
                Box::new(Expr::Value(json!("x"))),
            ),
            Locals { temps: &temps, env: Some(env) },
            &mut heap,
        )
        .unwrap();
        assert_eq!(value, number(2.0));
    }
}
