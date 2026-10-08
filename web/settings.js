/* DSH profile settings: provider routes, default model, and subagent selection. */
import { BrowserRuntime, httpPlugin } from '/agent/browser-runtime.js';

let runtime;
let http;
const byId = (id) => document.getElementById(id);
const view = { draft: null, revision: '', openProvider: null, tab: 'models', busy: false, probing: false };

function el(tag, className, label) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (label !== undefined) node.textContent = label;
  return node;
}

function status(message, error = false) {
  const node = byId('status');
  node.textContent = message;
  node.classList.toggle('error', error);
}

function showTab(tab) {
  const sections = runtime.entries('settings.section');
  const selected = sections.find((entry) => entry.id === tab);
  if (!selected) return;
  view.tab = tab;
  for (const entry of sections) {
    byId(entry.panel).hidden = entry.id !== tab;
    byId(entry.nav).classList.toggle('active', entry.id === tab);
  }
  byId('settings-footer').hidden = selected.hideFooter || false;
  byId('page-title').textContent = selected.title;
  selected.onShow?.();
}

async function loadPlugins() {
  const response = await http.request('/agent/plugins');
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || `读取插件状态失败 (${response.status})`);
  const byParent = new Map();
  for (const plugin of body.plugins || []) {
    const siblings = byParent.get(plugin.parent) || [];
    siblings.push(plugin); byParent.set(plugin.parent, siblings);
  }
  function branch(parent) {
    const list = el('ul');
    for (const plugin of byParent.get(parent) || []) {
      const item = el('li');
      const card = el('div', 'plugin-node');
      card.append(el('strong', '', `${plugin.id} · ${plugin.status}`));
      if (plugin.provides.length) card.append(el('small', '', `提供：${plugin.provides.join('、')}`));
      if (plugin.requires.length) card.append(el('small', '', `依赖：${plugin.requires.map((edge) => `${edge.service} ← #${edge.provider_scope}`).join('、')}`));
      item.append(card, branch(plugin.scope)); list.append(item);
    }
    return list;
  }
  const root = (body.plugins || []).find((plugin) => plugin.parent === null);
  byId('plugin-tree').replaceChildren(root ? branch(null) : el('p', 'field-note', '插件运行时尚未启动。'));
  const browserNodes = runtime.snapshot();
  const browserChildren = (parent) => {
    const list = el('ul');
    for (const plugin of browserNodes.filter((entry) => entry.parent === parent)) {
      const item = el('li');
      const card = el('div', 'plugin-node');
      card.append(el('strong', '', `${plugin.id} · ${plugin.status}`));
      if (plugin.provides.length) card.append(el('small', '', `提供：${plugin.provides.join('、')}`));
      item.append(card, browserChildren(plugin.id));
      list.append(item);
    }
    return list;
  };
  byId('browser-plugin-tree').replaceChildren(browserChildren(null));
  const events = (body.events || []).slice(-8).reverse();
  byId('plugin-events').replaceChildren(...events.map((entry) => el('div', '', `${entry.event} · ${entry.data?.id || ''}`)));
}

function textField(label, value, onInput, options = {}) {
  const wrapper = el('label');
  wrapper.append(el('span', '', label));
  const input = el(options.multiline ? 'textarea' : 'input');
  input.value = value || '';
  if (options.type) input.type = options.type;
  if (options.placeholder) input.placeholder = options.placeholder;
  if (options.readOnly) input.readOnly = true;
  input.addEventListener('input', () => onInput(input.value));
  wrapper.append(input);
  return wrapper;
}

function modelLines(provider) {
  return (provider.models || []).join('\n');
}

function parseModels(lines) {
  const models = [];
  const seen = new Set();
  for (const raw of lines.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (seen.has(line)) throw new Error(`模型 ID 重复：${line}`);
    seen.add(line); models.push(line);
  }
  if (!models.length) throw new Error('每个提供商至少需要一个模型 ID');
  return models;
}

function renderProviders() {
  const list = byId('provider-list');
  list.replaceChildren();
  if (!view.draft.providers.length) list.append(el('p', 'field-note', '还没有提供商。请先添加模型端点。'));
  for (const provider of view.draft.providers) {
    const card = el('article', 'provider-card');
    const row = el('div', 'provider-row');
    row.append(el('span', `key-dot${(provider.api_key || provider.has_api_key) && !provider.clear_api_key ? ' configured' : ''}`));
    const identity = el('div', 'provider-name');
    identity.append(el('span', '', provider.display_name || provider.id || '新提供商'), el('span', 'provider-tag', provider.api_type === 'anthropic-messages' ? 'Anthropic' : provider.api_type === 'openai-responses' ? 'Responses' : 'OpenAI'));
    row.append(identity);
    const toggle = el('button', 'small-button', view.openProvider === provider._draftId ? '收起' : '编辑');
    toggle.type = 'button';
    toggle.addEventListener('click', () => { view.openProvider = view.openProvider === provider._draftId ? null : provider._draftId; renderProviders(); });
    row.append(toggle);
    card.append(row, el('p', 'provider-summary', `${provider.url || '未设置端点'} · ${(provider.models || []).length} 个模型`));
    if (view.openProvider === provider._draftId) {
      const editor = el('div', 'provider-editor');
      editor.append(textField('Provider ID', provider.id, (value) => { provider.id = value; }, { readOnly: !provider._new, placeholder: '例如 moonshot' }));
      editor.append(textField('显示名称', provider.display_name, (value) => { provider.display_name = value; }, { placeholder: '例如 Moonshot AI' }));
      editor.append(textField('API 地址', provider.url, (value) => { provider.url = value; }, { placeholder: 'https://api.example.com/v1' }));
      const protocol = el('label');
      protocol.append(el('span', '', 'API 协议'));
      const select = el('select');
      for (const [value, label] of [['openai-completions', 'OpenAI Chat Completions'], ['openai-responses', 'OpenAI Responses'], ['anthropic-messages', 'Anthropic Messages']]) {
        const option = el('option', '', label); option.value = value; select.append(option);
      }
      select.value = provider.api_type;
      select.addEventListener('change', () => { provider.api_type = select.value; });
      protocol.append(select);
      editor.append(protocol);
      editor.append(textField('API 密钥', provider.api_key || '', (value) => { provider.api_key = value; provider.clear_api_key = false; }, { type: 'password', placeholder: provider.has_api_key ? '已配置；留空保持不变' : '输入密钥；无鉴权端点可留空' }));
      const clear = el('label');
      const checkbox = el('input'); checkbox.type = 'checkbox'; checkbox.checked = Boolean(provider.clear_api_key);
      checkbox.addEventListener('change', () => { provider.clear_api_key = checkbox.checked; if (checkbox.checked) provider.api_key = ''; });
      clear.append(checkbox, el('span', '', '清除已保存的密钥'));
      editor.append(clear);
      const modelsField = textField('模型目录（每行一个模型 ID）', provider.models_text, (value) => { provider.models_text = value; }, { multiline: true, placeholder: 'model-id' });
      editor.append(modelsField);
      const discover = el('button', 'small-button', '获取可用模型'); discover.type = 'button';
      const discovered = el('div', 'discovered-models');
      discover.addEventListener('click', async () => {
        discover.disabled = true; discovered.replaceChildren(el('span', 'field-note', '正在查询模型目录…'));
        try {
          const response = await http.request('/agent/settings/discover', {
            method: 'POST', headers: { 'content-type': 'application/json' },
            body: JSON.stringify({ id: provider.id, url: provider.url, ...(provider.api_key ? { api_key: provider.api_key } : {}) }),
          });
          const body = await response.json();
          if (!response.ok) throw new Error(body.error || `查询失败 (${response.status})`);
          discovered.replaceChildren();
          if (!body.models?.length) { discovered.append(el('span', 'field-note', '端点未返回可选模型。')); return; }
          const search = el('input'); search.type = 'search'; search.placeholder = '搜索模型 ID';
          const choices = el('select'); choices.multiple = true; choices.size = Math.min(8, body.models.length);
          const selected = new Set();
          choices.addEventListener('change', () => {
            for (const option of choices.options) {
              if (option.selected) selected.add(option.value);
              else selected.delete(option.value);
            }
          });
          const renderChoices = () => {
            choices.replaceChildren();
            for (const model of body.models.filter((id) => id.toLowerCase().includes(search.value.toLowerCase()))) {
              const option = el('option', '', model); option.value = model; option.selected = selected.has(model); choices.append(option);
            }
          };
          search.addEventListener('input', renderChoices); renderChoices();
          const add = el('button', 'small-button', '添加选中模型'); add.type = 'button';
          add.addEventListener('click', () => {
            const current = provider.models_text.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
            const known = new Set(current);
            for (const model of selected) if (!known.has(model)) { current.push(model); known.add(model); }
            provider.models_text = current.join('\n');
            modelsField.querySelector('textarea').value = provider.models_text;
            discovered.replaceChildren(el('span', 'field-note', '已加入草稿；点击页面底部的保存设置后生效。'));
          });
          discovered.append(search, choices, add);
        } catch (error) { discovered.replaceChildren(el('span', 'field-note', error.message)); }
        finally { discover.disabled = false; }
      });
      editor.append(discover, discovered);
      editor.append(el('p', 'field-note', '图片支持需实际验证。验证会先保存设置，再向选定模型发送一张随机测试图。'));
      for (const model of provider.models_text.split(/\r?\n/).map(line => line.trim()).filter(Boolean)) {
        const capability = provider.model_capabilities?.[model];
        const details = el('details');
        const label = { supported: '已验证支持', unsupported: '不支持', unknown: '未知' }[capability?.image_input || 'unknown'];
        details.append(el('summary', '', `${model} · 图片能力：${label}`));
        const probe = el('button', 'small-button', '验证图片输入'); probe.type = 'button';
        probe.disabled = view.probing || view.busy || provider.api_type !== 'openai-completions';
        probe.addEventListener('click', () => probeImage(provider.id.trim(), model));
        details.append(probe);
        if (capability?.image_probe) {
          const evidence = capability.image_probe;
          details.append(el('p', 'field-note', `${new Date(evidence.checked_at).toLocaleString()} · ${evidence.status} · ${evidence.elapsed_ms} ms`));
          const raw = el('pre', '', evidence.error || evidence.response_body || '');
          raw.style.whiteSpace = 'pre-wrap'; raw.style.overflowWrap = 'anywhere'; details.append(raw);
        }
        editor.append(details);
      }
      editor.append(el('p', 'field-note', `密钥引用：${provider.api_key_env || '保存密钥时自动创建'}。Provider ID 保存后固定。`));
      const actions = el('div', 'provider-actions');
      const remove = el('button', 'danger-button', '删除提供商'); remove.type = 'button';
      remove.addEventListener('click', () => {
        if (!confirm(`删除提供商 ${provider.id || '新提供商'}？保存后生效。`)) return;
        view.draft.providers = view.draft.providers.filter((item) => item !== provider);
        for (const [providerKey, modelKey] of [['orchestrator_provider', 'orchestrator_model'], ['expert_provider', 'expert_model']]) {
          if (view.draft[providerKey] === provider.id) {
            view.draft[providerKey] = null;
            view.draft[modelKey] = null;
          }
        }
        view.openProvider = null; renderProviders(); renderRoutes();
      });
      actions.append(remove); editor.append(actions); card.append(editor);
    }
    list.append(card);
  }
}

function fillSelect(id, selected) {
  const select = byId(id);
  select.replaceChildren();
  const empty = el('option', '', '未选择'); empty.value = ''; select.append(empty);
  for (const provider of view.draft.providers) {
    if (!provider.id.trim()) continue;
    const option = el('option', '', provider.id); option.value = provider.id; select.append(option);
  }
  select.value = selected || '';
}

function fillModelOptions(providerId, listId) {
  const list = byId(listId); list.replaceChildren();
  const provider = view.draft.providers.find((item) => item.id === providerId);
  if (!provider) return;
  try {
    for (const model of parseModels(provider.models_text)) {
      const option = el('option'); option.value = model; list.append(option);
    }
  } catch { /* Draft model lines are validated on save. */ }
}

function renderRoutes() {
  if (!view.draft) return;
  fillSelect('main-provider', view.draft.orchestrator_provider);
  fillSelect('expert-provider', view.draft.expert_provider);
  byId('main-model').value = view.draft.orchestrator_model || '';
  byId('expert-model').value = view.draft.expert_model || '';
  byId('enable-subagent').checked = Boolean(view.draft.enable_subagent);
  byId('subagent-override').open = Boolean(view.draft.expert_provider || view.draft.expert_model);
  fillModelOptions(view.draft.orchestrator_provider, 'main-model-options');
  fillModelOptions(view.draft.expert_provider, 'expert-model-options');
}

function adopt(data) {
  view.revision = data.revision;
  view.draft = {
    ...data,
    providers: (data.providers || []).map((provider, index) => ({ ...provider, _draftId: `saved-${index}`, _new: false, api_key: '', clear_api_key: false, models_text: modelLines(provider) })),
  };
  renderProviders(); renderRoutes();
}

async function load() {
  const response = await http.request('/agent/settings/data');
  const body = await response.json();
  if (!response.ok) throw new Error(body.error || `读取设置失败 (${response.status})`);
  adopt(body);
}

async function save() {
  if (view.busy || !view.draft) return;
  view.busy = true; byId('save-settings').disabled = true; status('正在保存…');
  try {
    const payload = {
      revision: view.revision,
      providers: view.draft.providers.map((provider) => ({
        id: provider.id.trim(), url: provider.url.trim(), api_type: provider.api_type, display_name: provider.display_name || null,
        models: parseModels(provider.models_text),
        ...(provider.api_key ? { api_key: provider.api_key } : {}),
        clear_api_key: provider.clear_api_key,
      })),
      orchestrator_provider: view.draft.orchestrator_provider || null,
      orchestrator_model: view.draft.orchestrator_model || null,
      expert_provider: view.draft.expert_provider || null,
      expert_model: view.draft.expert_model || null,
      enable_subagent: view.draft.enable_subagent,
    };
    const response = await http.request('/agent/settings/data', { method: 'PUT', headers: { 'content-type': 'application/json' }, body: JSON.stringify(payload) });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || `保存失败 (${response.status})`);
    adopt(body); status('已保存。新任务将使用当前模型路由。');
    return body;
  } catch (error) { status(error.message, true); }
  finally { view.busy = false; byId('save-settings').disabled = false; }
}

async function probeImage(provider, model) {
  if (view.probing || view.busy) return;
  view.probing = true;
  try {
    const saved = await save();
    if (!saved) return;
    byId('save-settings').disabled = true;
    status('正在验证图片输入…');
    const response = await http.request('/agent/settings/probe-image', { method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ revision: saved.revision, provider, model }) });
    const body = await response.json();
    if (!response.ok) throw new Error(body.error || `验证失败 (${response.status})`);
    view.revision = body.settings.revision;
    const current = view.draft.providers.find(item => item.id.trim() === provider);
    if (current) current.model_capabilities = { ...current.model_capabilities,
      [model]: body.settings.providers.find(item => item.id === provider).model_capabilities[model] };
    status(body.probe.capability === 'supported' ? '模型正确识别了测试图片，已接通真实图片输入。'
      : body.probe.error || '图片能力仍未确认。', body.probe.capability !== 'supported');
  } catch (error) { status(error.message, true); }
  finally { view.probing = false; renderProviders(); byId('save-settings').disabled = false; }
}

function initialize(ctx) {
  const theme = localStorage.getItem('agent-theme');
  if (theme === 'dark' || (!theme && matchMedia('(prefers-color-scheme: dark)').matches)) document.body.setAttribute('data-ds-dark-theme', '');
  ctx.listen(byId('theme-toggle'), 'click', () => { const dark = document.body.toggleAttribute('data-ds-dark-theme'); localStorage.setItem('agent-theme', dark ? 'dark' : 'light'); });
  for (const entry of runtime.entries('settings.section')) {
    ctx.listen(byId(entry.nav), 'click', () => showTab(entry.id));
  }
  ctx.listen(byId('refresh-plugins'), 'click', () => loadPlugins().catch((error) => status(error.message, true)));
  ctx.listen(byId('add-provider'), 'click', () => {
    if (!view.draft) return;
    let number = 1;
    while (view.draft.providers.some((item) => item.id === `provider-${number}`)) number++;
    const provider = { id: `provider-${number}`, _draftId: `new-${Date.now()}-${number}`, _new: true, url: '', api_type: 'openai-completions', models: [], models_text: '', has_api_key: false, api_key: '', clear_api_key: false };
    view.draft.providers.push(provider); view.openProvider = provider._draftId; renderProviders();
  });
  for (const [id, key] of [['main-provider', 'orchestrator_provider'], ['expert-provider', 'expert_provider']]) {
    ctx.listen(byId(id), 'change', () => {
      view.draft[key] = byId(id).value || null;
      const isMain = key === 'orchestrator_provider';
      const modelKey = isMain ? 'orchestrator_model' : 'expert_model';
      const modelId = isMain ? 'main-model' : 'expert-model';
      view.draft[modelKey] = null;
      byId(modelId).value = '';
      fillModelOptions(view.draft[key], isMain ? 'main-model-options' : 'expert-model-options');
    });
  }
  ctx.listen(byId('main-model'), 'input', () => { view.draft.orchestrator_model = byId('main-model').value; });
  ctx.listen(byId('expert-model'), 'input', () => { view.draft.expert_model = byId('expert-model').value; });
  ctx.listen(byId('enable-subagent'), 'change', () => { view.draft.enable_subagent = byId('enable-subagent').checked; });
  ctx.listen(byId('save-settings'), 'click', save);
  load().catch((error) => { status(error.message, true); byId('save-settings').disabled = true; });
}

async function boot() {
  runtime = new BrowserRuntime();
  await runtime.mount(httpPlugin);
  await runtime.mount({
    id: 'model-settings',
    start(ctx) { ctx.contribute('settings.section', { id: 'models', nav: 'nav-models', panel: 'models-panel', title: '模型提供商' }); },
  });
  await runtime.mount({
    id: 'agent-settings',
    start(ctx) { ctx.contribute('settings.section', { id: 'agents', nav: 'nav-agents', panel: 'agents-panel', title: '子代理', onShow: renderRoutes }); },
  });
  await runtime.mount({
    id: 'plugin-inspector',
    start(ctx) { ctx.contribute('settings.section', { id: 'plugins', nav: 'nav-plugins', panel: 'plugins-panel', title: '插件树', hideFooter: true, onShow: () => loadPlugins().catch((error) => status(error.message, true)) }); },
  });
  await runtime.mount({
    id: 'settings-shell', requires: ['http'],
    start(ctx) { http = ctx.get('http'); initialize(ctx); },
  });
  window.addEventListener('pagehide', () => runtime.dispose(), { once: true });
}

boot().catch((error) => {
  status(error.message, true);
  byId('save-settings').disabled = true;
  console.error('浏览器插件启动失败', error);
});
