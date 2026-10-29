//! Serializable identity-bearing values and lexical environments for the
//! JavaScript-subset interpreter. Closures are not yet connected to execution.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct HeapId(u64);

/// IEEE-754 bits are stored directly so non-finite values and signed zero
/// survive snapshots independently of JSON's number representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct JsNumber(u64);

impl JsNumber {
    pub(super) fn new(value: f64) -> Self {
        Self(value.to_bits())
    }
    pub(super) fn get(self) -> f64 {
        f64::from_bits(self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum VmValue {
    Undefined,
    Null,
    Boolean(bool),
    Number(JsNumber),
    String(String),
    Reference(HeapId),
    Promise(super::promise::PromiseId),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) enum HeapNode {
    Object(#[serde(deserialize_with = "deserialize_properties")] Vec<(String, VmValue)>),
    Array(Vec<VmValue>),
    Environment {
        parent: Option<HeapId>,
        bindings: BTreeMap<String, VmValue>,
        /// let/const names still in their temporal dead zone.
        #[serde(default)]
        uninitialized: BTreeSet<String>,
    },
    Closure {
        function: String,
        environment: HeapId,
    },
    /// Promise identity and reactions reside on the serialized heap.
    Promise {
        outcome: Option<super::promise::Outcome>,
        reactions: Vec<super::promise::ContinuationId>,
        handled: bool,
    },
    /// The shared already-resolved flag for a constructor's resolve/reject pair.
    PromiseCapability { promise: HeapId, resolved: bool },
    PromiseResolver { capability: HeapId, reject: bool },
    /// A built-in function or tool used as a value.
    Native(String),
}

fn deserialize_properties<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<(String, VmValue)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Properties {
        Ordered(Vec<(String, VmValue)>),
        Legacy(BTreeMap<String, VmValue>),
    }
    Ok(match Properties::deserialize(deserializer)? {
        Properties::Ordered(values) => values,
        Properties::Legacy(values) => values.into_iter().collect(),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Heap {
    next_id: u64,
    nodes: BTreeMap<HeapId, HeapNode>,
    #[serde(default)]
    array_holes: BTreeMap<HeapId, BTreeSet<usize>>,
    /// Objects created by a built-in constructor such as Error keep their
    /// class here, outside their enumerable properties.
    #[serde(default)]
    classes: BTreeMap<HeapId, String>,
    /// One node per built-in function value, so `String === String`.
    #[serde(default)]
    natives: BTreeMap<String, HeapId>,
}

// The existing interpreter will be migrated to these values incrementally.
#[allow(dead_code)]
impl Heap {
    /// Import a host JSON value into identity-bearing VM values. Each object
    /// or array becomes its own heap node; JSON itself cannot express aliases.
    pub(super) fn from_json(&mut self, input: &Value) -> Result<VmValue> {
        let first_new_id = self.next_id;
        let result = self.import_json(input, 0);
        if result.is_err() {
            self.nodes.retain(|id, _| id.0 < first_new_id);
            self.next_id = first_new_id;
        }
        result
    }

    fn import_json(&mut self, input: &Value, depth: usize) -> Result<VmValue> {
        if depth > 128 {
            bail!("JSON value exceeds maximum nesting depth");
        }
        Ok(match input {
            Value::Null => VmValue::Null,
            Value::Bool(value) => VmValue::Boolean(*value),
            Value::Number(value) => VmValue::Number(JsNumber::new(
                value.as_f64().context("invalid JSON number")?,
            )),
            Value::String(value) => VmValue::String(value.clone()),
            Value::Array(values) => {
                let values = values
                    .iter()
                    .map(|v| self.import_json(v, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                VmValue::Reference(self.allocate(HeapNode::Array(values))?)
            }
            Value::Object(values) => {
                let mut properties = Vec::new();
                for (key, value) in values {
                    properties.push((key.clone(), self.import_json(value, depth + 1)?));
                }
                VmValue::Reference(self.allocate(HeapNode::Object(properties))?)
            }
        })
    }

    /// Tool arguments and results use JSON. Inside objects and arrays this
    /// follows JSON.stringify: undefined and functions are omitted from
    /// objects and become null in arrays. A cycle, a non-finite number or a
    /// top-level value without a JSON form is an error.
    pub(super) fn to_json(&self, value: &VmValue) -> Result<Value> {
        self.export_json(value, &mut BTreeSet::new(), 0)
    }

    /// Values that JSON.stringify leaves out of objects and writes as null in
    /// arrays: undefined and functions.
    pub(super) fn json_absent(&self, value: &VmValue) -> bool {
        match value {
            VmValue::Undefined => true,
            VmValue::Reference(id) => matches!(
                self.nodes.get(id),
                Some(HeapNode::Closure { .. } | HeapNode::Native(_) | HeapNode::PromiseResolver { .. })
            ),
            _ => false,
        }
    }

    /// The function value of a built-in or tool, the same node for every use.
    pub(super) fn native(&mut self, name: &str) -> Result<HeapId> {
        if let Some(id) = self.natives.get(name) {
            return Ok(*id);
        }
        let id = self.allocate(HeapNode::Native(name.to_owned()))?;
        self.natives.insert(name.to_owned(), id);
        Ok(id)
    }

    /// Add an initialized binding to one environment, such as a hoisted function.
    pub(super) fn define_binding(&mut self, environment: HeapId, name: &str, value: VmValue) -> Result<()> {
        let (bindings, uninitialized) = self.environment_mut(environment)?;
        uninitialized.remove(name);
        bindings.insert(name.to_owned(), value);
        Ok(())
    }

    fn export_json(
        &self,
        value: &VmValue,
        active: &mut BTreeSet<HeapId>,
        depth: usize,
    ) -> Result<Value> {
        if depth > 128 {
            bail!("value exceeds maximum JSON nesting depth");
        }
        Ok(match value {
            VmValue::Undefined => bail!("undefined cannot cross the JSON host boundary"),
            VmValue::Null => Value::Null,
            VmValue::Boolean(value) => Value::Bool(*value),
            VmValue::Number(value) => {
                let number = value.get();
                if !number.is_finite() {
                    bail!("non-finite number cannot cross the JSON host boundary");
                }
                super::number_value(number)
            }
            VmValue::String(value) => Value::String(value.clone()),
            VmValue::Promise(_) => bail!("promise cannot cross the JSON host boundary"),
            VmValue::Reference(id) => {
                if !active.insert(*id) {
                    bail!("cyclic value cannot cross the JSON host boundary");
                }
                let result = match self.get(*id)? {
                    HeapNode::Object(properties) => {
                        let mut output = serde_json::Map::new();
                        for (key, item) in properties {
                            if !self.json_absent(item) {
                                output.insert(key.clone(), self.export_json(item, active, depth + 1)?);
                            }
                        }
                        Value::Object(output)
                    }
                    HeapNode::Array(items) => Value::Array(
                        items
                            .iter()
                            .enumerate()
                            .map(|(index, item)| {
                                if self.json_absent(item)
                                    || self
                                        .array_holes
                                        .get(id)
                                        .is_some_and(|holes| holes.contains(&index))
                                {
                                    Ok(Value::Null)
                                } else {
                                    self.export_json(item, active, depth + 1)
                                }
                            })
                            .collect::<Result<Vec<_>>>()?,
                    ),
                    HeapNode::Promise { .. } => bail!("promise cannot cross the JSON host boundary"),
                    HeapNode::Environment { .. } | HeapNode::Closure { .. } | HeapNode::Native(_) | HeapNode::PromiseCapability { .. } | HeapNode::PromiseResolver { .. } => {
                        bail!("function or environment cannot cross the JSON host boundary")
                    }
                };
                active.remove(id);
                result
            }
        })
    }

    pub(super) fn allocate(&mut self, node: HeapNode) -> Result<HeapId> {
        let id = HeapId(self.next_id);
        self.next_id = self.next_id.checked_add(1).context("heap id exhausted")?;
        self.nodes.insert(id, node);
        Ok(id)
    }

    pub(super) fn get(&self, id: HeapId) -> Result<&HeapNode> {
        self.nodes.get(&id).context("unknown heap reference")
    }
    pub(super) fn get_mut(&mut self, id: HeapId) -> Result<&mut HeapNode> {
        self.nodes.get_mut(&id).context("unknown heap reference")
    }

    /// Reclaim unreachable nodes between instructions, when the interpreter
    /// has no unsaved temporary Rust values on its stack.
    pub(super) fn collect(&mut self, roots: &[VmValue]) -> Result<usize> {
        let mut pending = Vec::new();
        for value in roots {
            if let VmValue::Reference(id) = value {
                pending.push(*id);
            }
        }
        // Built-in function values stay interned for the life of the program.
        pending.extend(self.natives.values().copied());
        let mut live = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !live.insert(id) {
                continue;
            }
            let mut visit = |value: &VmValue| {
                if let VmValue::Reference(id) = value {
                    pending.push(*id);
                }
            };
            match self.get(id)? {
                HeapNode::Object(properties) => {
                    for (_, value) in properties {
                        visit(value);
                    }
                }
                HeapNode::Array(values) => {
                    for value in values {
                        visit(value);
                    }
                }
                HeapNode::Environment {
                    parent, bindings, ..
                } => {
                    if let Some(parent) = parent {
                        pending.push(*parent);
                    }
                    for value in bindings.values() {
                        if let VmValue::Reference(id) = value {
                            pending.push(*id);
                        }
                    }
                }
                HeapNode::Closure { environment, .. } => pending.push(*environment),
                HeapNode::Promise { outcome, .. } => {
                    if let Some(super::promise::Outcome::Fulfilled(value) | super::promise::Outcome::Rejected(value)) = outcome { visit(value); }
                }
                HeapNode::PromiseCapability { promise, .. } => pending.push(*promise),
                HeapNode::PromiseResolver { capability, .. } => pending.push(*capability),
                HeapNode::Native(_) => {}
            }
        }
        let before = self.nodes.len();
        self.nodes.retain(|id, _| live.contains(id));
        self.array_holes.retain(|id, _| live.contains(id));
        self.classes.retain(|id, _| live.contains(id));
        Ok(before - self.nodes.len())
    }

    pub(super) fn append(&mut self, id: HeapId, items: Vec<VmValue>) -> Result<usize> {
        match self.nodes.get_mut(&id).context("unknown heap reference")? {
            HeapNode::Array(values) => {
                values.extend(items);
                Ok(values.len())
            }
            _ => bail!("result collection requires an array"),
        }
    }

    pub(super) fn set_property(&mut self, id: HeapId, key: String, value: VmValue) -> Result<()> {
        match self.nodes.get_mut(&id).context("unknown heap reference")? {
            HeapNode::Object(properties) => {
                if let Some((_, existing)) = properties.iter_mut().find(|(name, _)| name == &key) {
                    *existing = value;
                } else {
                    properties.push((key, value));
                }
                Ok(())
            }
            HeapNode::Array(items) => {
                if key == "length" {
                    let VmValue::Number(length) = value else {
                        bail!("array length must be a number")
                    };
                    let length = length.get();
                    if !length.is_finite()
                        || length < 0.0
                        || length.fract() != 0.0
                        || length > 1_000_000.0
                    {
                        bail!("invalid array length");
                    }
                    let length = length as usize;
                    let holes = self.array_holes.entry(id).or_default();
                    if length > items.len() {
                        holes.extend(items.len()..length);
                        items.resize(length, VmValue::Undefined);
                    } else {
                        items.truncate(length);
                        holes.retain(|index| *index < length);
                    }
                    return Ok(());
                }
                let index: usize = key.parse().context("array property must be an index")?;
                if index > 1_000_000 {
                    bail!("array index exceeds runtime limit");
                }
                let holes = self.array_holes.entry(id).or_default();
                if index >= items.len() {
                    holes.extend(items.len()..index);
                    items.resize(index + 1, VmValue::Undefined);
                }
                holes.remove(&index);
                items[index] = value;
                Ok(())
            }
            _ => bail!("property assignment requires an object"),
        }
    }

    pub(super) fn class_of(&self, id: HeapId) -> Option<&str> {
        self.classes.get(&id).map(String::as_str)
    }

    pub(super) fn set_class(&mut self, id: HeapId, class: &str) {
        self.classes.insert(id, class.to_owned());
    }

    /// A regular expression value is the object `{"$flow_regex": {pattern, flags}}`.
    pub(super) fn regex_parts(&self, id: HeapId) -> Option<(String, String)> {
        let HeapNode::Object(properties) = self.get(id).ok()? else {
            return None;
        };
        let [(key, VmValue::Reference(descriptor))] = properties.as_slice() else {
            return None;
        };
        if key != "$flow_regex" {
            return None;
        }
        let pattern = self.property(*descriptor, "pattern").ok()?;
        let flags = self.property(*descriptor, "flags").ok()?;
        match (pattern, flags) {
            (VmValue::String(pattern), VmValue::String(flags)) => Some((pattern, flags)),
            _ => None,
        }
    }

    /// Mark an index as a hole after removing or never assigning it.
    pub(super) fn set_hole(&mut self, id: HeapId, index: usize) {
        self.array_holes.entry(id).or_default().insert(index);
    }

    /// Replace an array's elements and holes in one step.
    pub(super) fn replace_array(&mut self, id: HeapId, items: Vec<VmValue>, holes: BTreeSet<usize>) -> Result<()> {
        match self.nodes.get_mut(&id).context("unknown heap reference")? {
            HeapNode::Array(values) => {
                *values = items;
                if holes.is_empty() {
                    self.array_holes.remove(&id);
                } else {
                    self.array_holes.insert(id, holes);
                }
                Ok(())
            }
            _ => bail!("receiver must be an array"),
        }
    }

    pub(super) fn holes(&self, id: HeapId) -> BTreeSet<usize> {
        self.array_holes.get(&id).cloned().unwrap_or_default()
    }

    /// The `delete` operator: an object loses the property, an array index
    /// becomes a hole. Other keys and values are left unchanged.
    pub(super) fn delete_property(&mut self, id: HeapId, key: &str) -> Result<()> {
        match self.nodes.get_mut(&id).context("unknown heap reference")? {
            HeapNode::Object(properties) => properties.retain(|(name, _)| name != key),
            HeapNode::Array(items) => {
                if let Ok(index) = key.parse::<usize>()
                    && index.to_string() == key
                    && index < items.len()
                {
                    items[index] = VmValue::Undefined;
                    self.array_holes.entry(id).or_default().insert(index);
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn has_array_index(&self, id: HeapId, index: usize) -> Result<bool> {
        let HeapNode::Array(items) = self.get(id)? else {
            bail!("receiver must be an array")
        };
        Ok(index < items.len()
            && !self
                .array_holes
                .get(&id)
                .is_some_and(|holes| holes.contains(&index)))
    }

    /// An object's own property, or None when it has no such property.
    pub(super) fn own_property(&self, id: HeapId, key: &str) -> Result<Option<VmValue>> {
        Ok(match self.get(id)? {
            HeapNode::Object(properties) => properties
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone()),
            _ => None,
        })
    }

    pub(super) fn property(&self, id: HeapId, key: &str) -> Result<VmValue> {
        match self.get(id)? {
            HeapNode::Object(properties) => Ok(properties
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or(VmValue::Undefined)),
            _ => bail!("property lookup requires an object"),
        }
    }

    /// A new environment whose `uninitialized` names stay in their temporal
    /// dead zone until declared.
    pub(super) fn new_environment(
        &mut self,
        parent: Option<HeapId>,
        bindings: BTreeMap<String, VmValue>,
        uninitialized: BTreeSet<String>,
    ) -> Result<HeapId> {
        let mut bindings = bindings;
        for name in &uninitialized {
            bindings.entry(name.clone()).or_insert(VmValue::Undefined);
        }
        self.allocate(HeapNode::Environment {
            parent,
            bindings,
            uninitialized,
        })
    }

    pub(super) fn environment_parent(&self, environment: HeapId) -> Result<Option<HeapId>> {
        match self.get(environment)? {
            HeapNode::Environment { parent, .. } => Ok(*parent),
            _ => bail!("reference is not a lexical environment"),
        }
    }

    /// A fresh copy with the same parent, for the per-iteration bindings of
    /// `for (let …)`: closures made in earlier iterations keep the old copy.
    pub(super) fn copy_environment(&mut self, environment: HeapId) -> Result<HeapId> {
        let node = match self.get(environment)? {
            node @ HeapNode::Environment { .. } => node.clone(),
            _ => bail!("reference is not a lexical environment"),
        };
        self.allocate(node)
    }

    /// Bindings of one environment that have been initialized, without its parents.
    pub(super) fn initialized_bindings(&self, environment: HeapId) -> Result<Vec<(String, VmValue)>> {
        match self.get(environment)? {
            HeapNode::Environment {
                bindings,
                uninitialized,
                ..
            } => Ok(bindings
                .iter()
                .filter(|(name, _)| !uninitialized.contains(*name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()),
            _ => bail!("reference is not a lexical environment"),
        }
    }

    /// The environment that binds `name`, innermost first.
    fn binding_owner(&self, mut environment: HeapId, name: &str) -> Result<HeapId> {
        // A cycle can only arise from an invalid or corrupted snapshot.
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(environment) {
                bail!("cyclic lexical environment");
            }
            match self.get(environment)? {
                HeapNode::Environment {
                    parent, bindings, ..
                } => {
                    if bindings.contains_key(name) {
                        return Ok(environment);
                    }
                    environment = match parent {
                        Some(parent) => *parent,
                        None => bail!("ReferenceError: {name} is not defined"),
                    };
                }
                _ => bail!("reference is not a lexical environment"),
            }
        }
    }

    fn environment_mut(
        &mut self,
        environment: HeapId,
    ) -> Result<(&mut BTreeMap<String, VmValue>, &mut BTreeSet<String>)> {
        match self
            .nodes
            .get_mut(&environment)
            .context("unknown heap reference")?
        {
            HeapNode::Environment {
                bindings,
                uninitialized,
                ..
            } => Ok((bindings, uninitialized)),
            _ => bail!("reference is not a lexical environment"),
        }
    }

    pub(super) fn lookup_binding(&self, environment: HeapId, name: &str) -> Result<VmValue> {
        let owner = self.binding_owner(environment, name)?;
        let HeapNode::Environment {
            bindings,
            uninitialized,
            ..
        } = self.get(owner)?
        else {
            bail!("reference is not a lexical environment")
        };
        if uninitialized.contains(name) {
            bail!("ReferenceError: Cannot access '{name}' before initialization");
        }
        Ok(bindings.get(name).cloned().unwrap_or(VmValue::Undefined))
    }

    /// Assignment to an existing binding; a let/const in its dead zone throws.
    pub(super) fn assign_binding(
        &mut self,
        environment: HeapId,
        name: &str,
        value: VmValue,
    ) -> Result<()> {
        let owner = self.binding_owner(environment, name)?;
        let (bindings, uninitialized) = self.environment_mut(owner)?;
        if uninitialized.contains(name) {
            bail!("ReferenceError: Cannot access '{name}' before initialization");
        }
        bindings.insert(name.to_owned(), value);
        Ok(())
    }

    /// The declaration itself: initialize the nearest binding and end its dead zone.
    pub(super) fn initialize_binding(
        &mut self,
        environment: HeapId,
        name: &str,
        value: VmValue,
    ) -> Result<()> {
        let owner = self.binding_owner(environment, name)?;
        let (bindings, uninitialized) = self.environment_mut(owner)?;
        uninitialized.remove(name);
        bindings.insert(name.to_owned(), value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn previous_object_snapshot_format_remains_readable() {
        let value = json!({"next_id":1,"nodes":{"0":{"Object":{"x":"Null"}}}});
        let heap: Heap = serde_json::from_value(value).unwrap();
        assert_eq!(heap.property(HeapId(0), "x").unwrap(), VmValue::Null);
    }

    #[test]
    fn collection_preserves_reachable_cycles_and_removes_unreachable_nodes() {
        let mut heap = Heap::default();
        let live = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let dead = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        heap.set_property(live, "self".into(), VmValue::Reference(live))
            .unwrap();
        heap.set_property(dead, "self".into(), VmValue::Reference(dead))
            .unwrap();
        assert_eq!(heap.collect(&[VmValue::Reference(live)]).unwrap(), 1);
        assert!(heap.get(live).is_ok());
        assert!(heap.get(dead).is_err());
    }

    #[test]
    fn javascript_numbers_survive_snapshot_without_json_number_loss() {
        let heap = Heap::default();
        for number in [-0.0, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let original = VmValue::Number(JsNumber::new(number));
            let encoded = serde_json::to_vec(&original).unwrap();
            let restored: VmValue = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(original, restored);
            if !number.is_finite() {
                assert!(heap.to_json(&restored).is_err());
            }
        }
    }

    #[test]
    fn failed_import_does_not_leave_partial_heap_nodes() {
        let mut heap = Heap::default();
        let existing = heap.from_json(&json!({"existing": true})).unwrap();
        let initial_count = heap.nodes.len();
        let mut deep = json!(0);
        for _ in 0..130 {
            deep = json!([deep]);
        }
        assert!(heap.from_json(&json!([{}, deep])).is_err());
        assert_eq!(heap.nodes.len(), initial_count);
        assert_eq!(heap.to_json(&existing).unwrap(), json!({"existing": true}));
    }

    #[test]
    fn json_boundary_preserves_shared_references_and_rejects_cycles() {
        let mut heap = Heap::default();
        let shared = heap
            .from_json(&json!({"name":"before", "items":[1,2]}))
            .unwrap();
        let id = match shared {
            VmValue::Reference(id) => id,
            _ => panic!("expected object"),
        };
        let alias = shared.clone();
        heap.set_property(id, "name".into(), VmValue::String("after".into()))
            .unwrap();
        let root = VmValue::Reference(heap.allocate(HeapNode::Array(vec![shared, alias])).unwrap());
        let restored: Heap = serde_json::from_slice(&serde_json::to_vec(&heap).unwrap()).unwrap();
        assert_eq!(
            restored.to_json(&root).unwrap(),
            json!([
                {"name":"after", "items":[1,2]}, {"name":"after", "items":[1,2]}
            ])
        );
        heap.set_property(id, "self".into(), VmValue::Reference(id))
            .unwrap();
        assert!(
            heap.to_json(&root)
                .unwrap_err()
                .to_string()
                .contains("cyclic")
        );
        assert!(heap.to_json(&VmValue::Undefined).is_err());
    }

    #[test]
    fn json_boundary_follows_json_stringify_for_undefined() {
        // As JSON.stringify: omitted from objects, null in arrays.
        let mut heap = Heap::default();
        let object = heap
            .allocate(HeapNode::Object(vec![
                ("kept".into(), VmValue::Null),
                ("dropped".into(), VmValue::Undefined),
            ]))
            .unwrap();
        let array = heap
            .allocate(HeapNode::Array(vec![
                VmValue::Undefined,
                VmValue::Reference(object),
            ]))
            .unwrap();
        assert_eq!(
            heap.to_json(&VmValue::Reference(array)).unwrap(),
            json!([null, {"kept": null}])
        );
    }

    #[test]
    fn shared_object_identity_and_cycles_survive_restore() {
        let mut heap = Heap::default();
        let object = heap.allocate(HeapNode::Object(Vec::new())).unwrap();
        let alias = object;
        heap.set_property(object, "self".into(), VmValue::Reference(object))
            .unwrap();
        heap.set_property(alias, "value".into(), VmValue::String("updated".into()))
            .unwrap();
        let bytes = serde_json::to_vec(&heap).unwrap();
        let restored: Heap = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            restored.property(object, "self").unwrap(),
            VmValue::Reference(object)
        );
        assert_eq!(
            restored.property(object, "value").unwrap(),
            VmValue::String("updated".into())
        );
        assert_eq!(
            restored.property(object, "missing").unwrap(),
            VmValue::Undefined
        );
    }

    #[test]
    fn closures_share_captured_bindings_after_restore() {
        let mut heap = Heap::default();
        let outer = heap
            .new_environment(
                None,
                [("count".into(), VmValue::Number(JsNumber::new(0.0)))].into(),
                BTreeSet::new(),
            )
            .unwrap();
        let inner = heap
            .new_environment(Some(outer), BTreeMap::new(), BTreeSet::new())
            .unwrap();
        let closure = heap
            .allocate(HeapNode::Closure {
                function: "increment".into(),
                environment: inner,
            })
            .unwrap();
        let bytes = serde_json::to_vec(&heap).unwrap();
        let mut restored: Heap = serde_json::from_slice(&bytes).unwrap();
        restored
            .assign_binding(inner, "count", VmValue::Number(JsNumber::new(1.0)))
            .unwrap();
        assert_eq!(
            restored.lookup_binding(outer, "count").unwrap(),
            VmValue::Number(JsNumber::new(1.0))
        );
        assert_eq!(
            restored.get(closure).unwrap(),
            &HeapNode::Closure {
                function: "increment".into(),
                environment: inner
            }
        );
    }

    #[test]
    fn dead_zone_shadowing_and_per_iteration_copies() {
        let mut heap = Heap::default();
        let outer = heap
            .new_environment(None, [("x".into(), VmValue::String("outer".into()))].into(), BTreeSet::new())
            .unwrap();
        // An inner let shadows the outer binding even before it is declared.
        let inner = heap
            .new_environment(Some(outer), BTreeMap::new(), ["x".to_owned()].into())
            .unwrap();
        let error = heap.lookup_binding(inner, "x").unwrap_err().to_string();
        assert!(error.contains("before initialization"), "{error}");
        assert!(heap.assign_binding(inner, "x", VmValue::Null).is_err());
        heap.initialize_binding(inner, "x", VmValue::String("inner".into())).unwrap();
        assert_eq!(heap.lookup_binding(inner, "x").unwrap(), VmValue::String("inner".into()));
        assert_eq!(heap.lookup_binding(outer, "x").unwrap(), VmValue::String("outer".into()));
        let copy = heap.copy_environment(inner).unwrap();
        heap.assign_binding(copy, "x", VmValue::String("next".into())).unwrap();
        assert_eq!(heap.lookup_binding(inner, "x").unwrap(), VmValue::String("inner".into()));
        assert_eq!(heap.environment_parent(copy).unwrap(), Some(outer));
        let missing = heap.lookup_binding(inner, "nothing").unwrap_err().to_string();
        assert!(missing.starts_with("ReferenceError: nothing is not defined"), "{missing}");
    }
}
