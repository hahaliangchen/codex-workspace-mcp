/* Browser companion for the Rust plugin tree. Built-in UI plugins only. */
export class BrowserRuntime {
  constructor() {
    this.root = { id: 'browser', parent: null, children: [], disposers: [], status: 'active' };
    this.scopes = [this.root];
    this.services = new Map();
    this.slots = new Map();
    this.listeners = new Map();
  }

  lookup(scope, name) {
    for (let cursor = scope; cursor; cursor = cursor.parent) {
      const service = this.services.get(name)?.find((entry) => entry.visibleIn === cursor);
      if (service) return service.value;
    }
    throw new Error(`浏览器插件缺少服务：${name}`);
  }

  entries(name) {
    return [...(this.slots.get(name) || [])]
      .sort((a, b) => (a.value.priority || 0) - (b.value.priority || 0))
      .map((entry) => entry.value);
  }

  emit(name, data) {
    for (const entry of [...(this.listeners.get(name) || [])]) entry.callback(data);
  }

  async mount(plugin, parent = this.root) {
    if (parent.status !== 'active' && parent.status !== 'starting') {
      throw new Error(`父作用域未运行：${parent.id}`);
    }
    if (this.scopes.some((scope) => scope.parent === parent && scope.id === plugin.id)) {
      throw new Error(`浏览器插件重复：${plugin.id}`);
    }
    for (const service of plugin.requires || []) this.lookup(parent, service);
    const scope = { id: plugin.id, parent, children: [], disposers: [], status: 'starting' };
    parent.children.push(scope);
    this.scopes.push(scope);
    const effect = (dispose) => {
      if (typeof dispose !== 'function') throw new Error('清理函数必须可调用');
      scope.disposers.push(dispose);
      return dispose;
    };
    const context = {
      get: (name) => this.lookup(scope, name),
      provide: (name, value) => {
        if (this.services.get(name)?.some((entry) => entry.visibleIn === parent)) {
          throw new Error(`服务重复：${name}`);
        }
        const entry = { owner: scope, visibleIn: parent, value };
        const list = this.services.get(name) || [];
        list.push(entry);
        this.services.set(name, list);
        effect(() => {
          const current = this.services.get(name);
          const index = current?.indexOf(entry) ?? -1;
          if (index >= 0) current.splice(index, 1);
          if (!current?.length) this.services.delete(name);
        });
      },
      contribute: (name, value) => {
        const entry = { owner: scope, value };
        const list = this.slots.get(name) || [];
        if (list.some((item) => item.value.id === value.id)) throw new Error(`插槽条目重复：${name}/${value.id}`);
        list.push(entry);
        this.slots.set(name, list);
        effect(() => {
          const current = this.slots.get(name);
          const index = current?.indexOf(entry) ?? -1;
          if (index >= 0) current.splice(index, 1);
          if (!current?.length) this.slots.delete(name);
        });
      },
      on: (name, callback) => {
        const entry = { owner: scope, callback };
        const list = this.listeners.get(name) || [];
        list.push(entry);
        this.listeners.set(name, list);
        effect(() => {
          const current = this.listeners.get(name);
          const index = current?.indexOf(entry) ?? -1;
          if (index >= 0) current.splice(index, 1);
          if (!current?.length) this.listeners.delete(name);
        });
      },
      emit: (name, data) => this.emit(name, data),
      effect,
      listen: (target, name, callback) => {
        target.addEventListener(name, callback);
        effect(() => target.removeEventListener(name, callback));
      },
      interval: (callback, milliseconds) => {
        const timer = setInterval(callback, milliseconds);
        effect(() => clearInterval(timer));
      },
      mount: (child) => this.mount(child, scope),
    };
    try {
      const dispose = await plugin.start(context);
      if (typeof dispose === 'function') effect(dispose);
      scope.status = 'active';
      this.emit('plugin/active', { id: scope.id });
      return scope;
    } catch (error) {
      this.dispose(scope);
      throw error;
    }
  }

  dispose(scope = this.root) {
    if (scope.status === 'stopped') return;
    scope.status = 'stopping';
    for (const child of [...scope.children].reverse()) this.dispose(child);
    for (const dispose of scope.disposers.reverse()) {
      try { dispose(); } catch (error) { console.error(`清理浏览器插件 ${scope.id} 失败`, error); }
    }
    scope.disposers.length = 0;
    if (scope.parent) {
      const index = scope.parent.children.indexOf(scope);
      if (index >= 0) scope.parent.children.splice(index, 1);
    }
    if (scope !== this.root) {
      const index = this.scopes.indexOf(scope);
      if (index >= 0) this.scopes.splice(index, 1);
    }
    scope.status = 'stopped';
    this.emit('plugin/stopped', { id: scope.id });
  }

  snapshot() {
    return this.scopes.map((scope) => ({
      id: scope.id,
      parent: scope.parent?.id || null,
      status: scope.status,
      provides: [...this.services].filter(([, entries]) => entries.some((entry) => entry.owner === scope)).map(([name]) => name),
    }));
  }
}

export const httpPlugin = {
  id: 'rust-api',
  start(ctx) {
    ctx.provide('http', {
      request: (path, options) => fetch(path, options),
      async json(path, options) {
        const response = await fetch(path, options);
        const body = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(body.error || `请求失败 (${response.status})`);
        return body;
      },
      events: (path) => new EventSource(path),
    });
  },
};
