// Works inside the host's opaque sandbox. The host validates page ownership.
(() => {
  const pending = new Map();
  let sequence = 0;
  window.addEventListener('message', event => {
    if (event.source !== parent || event.data?.type !== 'skill-package/response') return;
    const request = pending.get(event.data.id);
    if (!request) return;
    pending.delete(event.data.id);
    clearTimeout(request.timer);
    if (event.data.error) request.reject(new Error(event.data.error));
    else request.resolve(event.data.result);
  });
  function request(method, details = {}) {
    return new Promise((resolve, reject) => {
      const id = `page-${++sequence}`;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error('Skill page request timed out'));
      }, 610000);
      pending.set(id, { resolve, reject, timer });
      parent.postMessage({ type: 'skill-package/request', id, method, ...details }, '*');
    });
  }
  window.skillPackage = Object.freeze({
    workspace: () => request('workspace'),
    callTool: (name, args = {}) => request('tool', { name, arguments: args }),
    session: (operation, args = {}) => request('session', { operation, arguments: args }),
    service: (operation, args = {}) => request('service', { operation, arguments: args }),
  });
})();
