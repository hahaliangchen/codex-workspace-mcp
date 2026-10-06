//! A small, Rust-native Cordis-style runtime for built-in capabilities.
//! Scopes form a tree; service dependencies may cross sibling scopes.

use std::{
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result, anyhow, ensure};
use serde::Serialize;
use serde_json::{Value, json};

pub type ScopeId = u64;
pub const ROOT: ScopeId = 0;

type ServiceValue = Arc<dyn Any + Send + Sync>;
type Disposer = Box<dyn FnOnce() + Send>;
type Listener = Arc<dyn Fn(&Value) + Send + Sync>;

pub trait Plugin: Send + Sync {
    fn id(&self) -> &'static str;
    fn requires(&self) -> &'static [&'static str] {
        &[]
    }
    fn start(&self, context: &PluginContext<'_>) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Starting,
    Active,
    Stopping,
}

struct Scope {
    id: String,
    parent: Option<ScopeId>,
    children: Vec<ScopeId>,
    requires: Vec<(String, ScopeId)>,
    provides: Vec<String>,
    disposers: Vec<Disposer>,
    status: Status,
}

struct Service {
    visible_in: ScopeId,
    owner: ScopeId,
    value: ServiceValue,
}

struct EventListener {
    owner: ScopeId,
    callback: Listener,
}

struct Inner {
    next_scope: ScopeId,
    scopes: BTreeMap<ScopeId, Scope>,
    services: HashMap<String, Vec<Service>>,
    listeners: HashMap<String, Vec<EventListener>>,
}

impl Inner {
    fn is_inside(&self, mut scope: ScopeId, owner: ScopeId) -> bool {
        loop {
            if scope == owner {
                return true;
            }
            let Some(parent) = self.scopes.get(&scope).and_then(|node| node.parent) else {
                return false;
            };
            scope = parent;
        }
    }

    fn lookup(&self, scope: ScopeId, key: &str) -> Option<&Service> {
        let mut cursor = Some(scope);
        while let Some(id) = cursor {
            if let Some(value) = self.services.get(key).and_then(|entries| {
                entries.iter().rev().find(|entry| {
                    entry.visible_in == id
                        && self.scopes.get(&entry.owner).is_some_and(|owner| {
                            owner.status == Status::Active
                                || (owner.status == Status::Starting
                                    && self.is_inside(scope, entry.owner))
                        })
                })
            }) {
                return Some(value);
            }
            cursor = self.scopes.get(&id)?.parent;
        }
        None
    }

    fn subtree(&self, scope: ScopeId, output: &mut Vec<ScopeId>) {
        if let Some(node) = self.scopes.get(&scope) {
            for child in node.children.iter().rev() {
                self.subtree(*child, output);
            }
            output.push(scope);
        }
    }
}

pub struct PluginRuntime {
    inner: Mutex<Inner>,
}

pub struct PluginContext<'a> {
    runtime: &'a PluginRuntime,
    scope: ScopeId,
}

#[derive(Debug, Serialize)]
pub struct DependencyView {
    pub service: String,
    pub provider_scope: ScopeId,
}

#[derive(Debug, Serialize)]
pub struct PluginView {
    pub scope: ScopeId,
    pub id: String,
    pub parent: Option<ScopeId>,
    pub status: String,
    pub requires: Vec<DependencyView>,
    pub provides: Vec<String>,
}

impl PluginRuntime {
    pub fn new() -> Self {
        let mut scopes = BTreeMap::new();
        scopes.insert(
            ROOT,
            Scope {
                id: "root".into(),
                parent: None,
                children: Vec::new(),
                requires: Vec::new(),
                provides: Vec::new(),
                disposers: Vec::new(),
                status: Status::Active,
            },
        );
        Self {
            inner: Mutex::new(Inner {
                next_scope: 1,
                scopes,
                services: HashMap::new(),
                listeners: HashMap::new(),
            }),
        }
    }

    pub fn mount<P: Plugin>(&self, parent: ScopeId, plugin: P) -> Result<ScopeId> {
        let id = plugin.id();
        let scope = {
            let mut inner = self.inner.lock().unwrap();
            ensure!(
                inner.scopes.contains_key(&parent),
                "parent plugin scope {parent} does not exist"
            );
            ensure!(
                inner.scopes.values().all(|node| node.id != id),
                "plugin {id} is already mounted"
            );
            let dependencies = plugin
                .requires()
                .iter()
                .map(|key| {
                    inner
                        .lookup(parent, key)
                        .map(|service| ((*key).to_owned(), service.owner))
                        .ok_or_else(|| anyhow!("plugin {id} requires unavailable service {key}"))
                })
                .collect::<Result<Vec<_>>>()?;
            let scope = inner.next_scope;
            inner.next_scope += 1;
            inner.scopes.insert(
                scope,
                Scope {
                    id: id.into(),
                    parent: Some(parent),
                    children: Vec::new(),
                    requires: dependencies,
                    provides: Vec::new(),
                    disposers: Vec::new(),
                    status: Status::Starting,
                },
            );
            inner.scopes.get_mut(&parent).unwrap().children.push(scope);
            scope
        };
        let context = PluginContext {
            runtime: self,
            scope,
        };
        if let Err(error) = plugin
            .start(&context)
            .with_context(|| format!("starting plugin {id}"))
        {
            let _ = self.unmount(scope);
            return Err(error);
        }
        self.inner
            .lock()
            .unwrap()
            .scopes
            .get_mut(&scope)
            .unwrap()
            .status = Status::Active;
        self.emit(
            "plugin/started",
            json!({"id":id,"scope":scope,"parent":parent}),
        );
        Ok(scope)
    }

    pub fn service<T: Any + Send + Sync>(&self, scope: ScopeId, key: &str) -> Result<Arc<T>> {
        let inner = self.inner.lock().unwrap();
        let value = inner
            .lookup(scope, key)
            .ok_or_else(|| anyhow!("service {key} is unavailable in scope {scope}"))?
            .value
            .clone();
        Arc::downcast::<T>(value).map_err(|_| anyhow!("service {key} has a different type"))
    }

    pub fn unmount(&self, scope: ScopeId) -> Result<()> {
        ensure!(scope != ROOT, "root scope cannot be unmounted");
        let order = {
            let inner = self.inner.lock().unwrap();
            ensure!(
                inner.scopes.contains_key(&scope),
                "plugin scope {scope} does not exist"
            );
            let mut order = Vec::new();
            inner.subtree(scope, &mut order);
            let removed: HashSet<_> = order.iter().copied().collect();
            for (id, node) in &inner.scopes {
                if removed.contains(id) {
                    continue;
                }
                if let Some((key, _)) = node
                    .requires
                    .iter()
                    .find(|(_, owner)| removed.contains(owner))
                {
                    return Err(anyhow!(
                        "cannot unload plugin: {} still depends on service {key}",
                        node.id
                    ));
                }
            }
            order
        };
        for current in order {
            let (id, disposers) = {
                let mut inner = self.inner.lock().unwrap();
                let node = inner.scopes.get_mut(&current).unwrap();
                node.status = Status::Stopping;
                (node.id.clone(), std::mem::take(&mut node.disposers))
            };
            for dispose in disposers.into_iter().rev() {
                dispose();
            }
            let mut inner = self.inner.lock().unwrap();
            inner
                .services
                .values_mut()
                .for_each(|entries| entries.retain(|entry| entry.owner != current));
            inner
                .listeners
                .values_mut()
                .for_each(|entries| entries.retain(|entry| entry.owner != current));
            let parent = inner.scopes.remove(&current).and_then(|node| node.parent);
            if let Some(parent) = parent {
                if let Some(node) = inner.scopes.get_mut(&parent) {
                    node.children.retain(|child| *child != current);
                }
            }
            drop(inner);
            self.emit("plugin/stopped", json!({"id":id,"scope":current}));
        }
        Ok(())
    }

    pub fn shutdown(&self) -> Result<()> {
        let children = self
            .inner
            .lock()
            .unwrap()
            .scopes
            .get(&ROOT)
            .unwrap()
            .children
            .clone();
        for child in children.into_iter().rev() {
            self.unmount(child)?;
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<PluginView> {
        self.inner
            .lock()
            .unwrap()
            .scopes
            .iter()
            .map(|(scope, node)| PluginView {
                scope: *scope,
                id: node.id.clone(),
                parent: node.parent,
                status: format!("{:?}", node.status).to_lowercase(),
                requires: node
                    .requires
                    .iter()
                    .map(|(key, owner)| DependencyView {
                        service: key.clone(),
                        provider_scope: *owner,
                    })
                    .collect(),
                provides: node.provides.clone(),
            })
            .collect()
    }

    pub fn emit(&self, event: &str, data: Value) {
        let listeners = self
            .inner
            .lock()
            .unwrap()
            .listeners
            .get(event)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.callback.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for listener in listeners {
            listener(&data);
        }
    }
}

impl PluginContext<'_> {
    pub fn require<T: Any + Send + Sync>(&self, key: &str) -> Result<Arc<T>> {
        self.runtime.service(self.scope, key)
    }

    pub fn provide<T: Any + Send + Sync>(&self, key: &'static str, value: Arc<T>) -> Result<()> {
        let mut inner = self.runtime.inner.lock().unwrap();
        let parent = inner
            .scopes
            .get(&self.scope)
            .and_then(|node| node.parent)
            .ok_or_else(|| anyhow!("plugin scope {} is unavailable", self.scope))?;
        ensure!(
            inner
                .services
                .get(key)
                .is_none_or(|entries| entries.iter().all(|entry| entry.visible_in != parent)),
            "service {key} is already provided in this scope"
        );
        inner.services.entry(key.into()).or_default().push(Service {
            visible_in: parent,
            owner: self.scope,
            value,
        });
        inner
            .scopes
            .get_mut(&self.scope)
            .unwrap()
            .provides
            .push(key.into());
        Ok(())
    }

    pub fn effect(&self, disposer: impl FnOnce() + Send + 'static) {
        if let Some(node) = self
            .runtime
            .inner
            .lock()
            .unwrap()
            .scopes
            .get_mut(&self.scope)
        {
            node.disposers.push(Box::new(disposer));
        }
    }

    pub fn on(&self, event: &'static str, callback: impl Fn(&Value) + Send + Sync + 'static) {
        self.runtime
            .inner
            .lock()
            .unwrap()
            .listeners
            .entry(event.into())
            .or_default()
            .push(EventListener {
                owner: self.scope,
                callback: Arc::new(callback),
            });
    }

    pub fn mount<P: Plugin>(&self, plugin: P) -> Result<ScopeId> {
        self.runtime.mount(self.scope, plugin)
    }
}
