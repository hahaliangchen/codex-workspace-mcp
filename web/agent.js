/* A small browser adapter for the Rust task API. Event shapes are read from
 * persisted records so the same renderer handles live delivery and refresh. */
import { BrowserRuntime, httpPlugin } from '/agent/browser-runtime.js';

let runtime;
let http;
const byId = (id) => document.getElementById(id);
const welcome = byId('welcome');
const view = {
  tasks: [],
  selected: null,
  events: [],
  tab: 'chat',
  ready: false,
  stream: null,
  error: '',
  busy: false,
};

const terminalStatuses = new Set(['completed', 'failed', 'cancelled', 'max_steps', 'interrupted']);
const statusNames = {
  running: '运行中', completed: '已完成', failed: '失败', cancelled: '已取消',
  cancelling: '正在停止', max_steps: '已达到步数上限', interrupted: '已中断',
};

async function api(path, options) {
  return http.json(path, options);
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function clock(time) {
  return typeof time === 'number' ? new Date(time).toLocaleTimeString('zh-CN', { hour12: false }) : '';
}

function relativeTime(time) {
  if (!time) return '';
  const seconds = Math.max(0, Math.floor((Date.now() - time) / 1000));
  if (seconds < 60) return '刚刚';
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return new Date(time).toLocaleDateString('zh-CN');
}

function textBlocks(content) {
  if (typeof content === 'string') return content;
  if (!Array.isArray(content)) return '';
  return content.map((part) => {
    if (part?.type === 'text' || part?.type === 'reasoning') return part.text || '';
    if (part?.type === 'tool-result') return textBlocks(part.content);
    return '';
  }).filter(Boolean).join('\n');
}

function messageOf(event) {
  const data = event?.data || {};
  return data.message || data;
}

function pretty(value) {
  if (value === undefined || value === null) return '';
  if (typeof value !== 'string') return JSON.stringify(value, null, 2);
  try { return JSON.stringify(JSON.parse(value), null, 2); } catch { return value; }
}

function preview(text, limit = 12000) {
  if (!text || text.length <= limit) return text || '';
  return `${text.slice(0, limit)}\n…（已截断，完整内容保存在任务事件中）`;
}

function currentTask() {
  return view.tasks.find((task) => task.task_id === view.selected);
}

function closeStream() {
  if (view.stream) view.stream.close();
  view.stream = null;
}

function setTab(tab) {
  if (!runtime.entries('task.view').some((entry) => entry.id === tab)) return;
  view.tab = tab;
  for (const entry of runtime.entries('task.view')) {
    const node = byId(entry.button);
    node.classList.toggle('active', entry.id === tab);
    node.setAttribute('aria-selected', String(entry.id === tab));
  }
  renderEvents();
}

function renderTaskList() {
  const list = byId('task-list');
  list.replaceChildren();
  if (view.tasks.length === 0) {
    list.append(element('div', 'history-empty', '还没有任务'));
    return;
  }
  for (const task of view.tasks) {
    const button = element('button', `task-item${task.task_id === view.selected ? ' selected' : ''}${task.parent_task_id ? ' child-task' : ''}`);
    button.type = 'button';
    button.title = task.prompt;
    button.append(element('span', 'task-item-icon', task.parent_task_id ? '↳' : '▤'));
    const body = element('span', 'task-item-body');
    body.append(element('span', 'task-item-title', task.prompt || '未命名任务'));
    body.append(element('span', 'task-item-meta', `${task.parent_task_id ? '子代理 · ' : ''}${statusNames[task.status] || task.status} · ${relativeTime(task.updated_at)}`));
    button.append(body);
    button.addEventListener('click', () => selectTask(task.task_id));
    list.append(button);
  }
}

function renderStatus() {
  const task = currentTask();
  byId('task-title').textContent = task ? task.prompt : '新任务';
  byId('parent-task').hidden = !task?.parent_task_id;
  const status = byId('task-status');
  status.textContent = task ? statusNames[task.status] || task.status : '';
  status.className = `task-status ${task?.status || ''}`;
  byId('cancel-task').hidden = task?.status !== 'running';
  byId('composer-note').textContent = task?.parent_task_id ? '子代理轨迹只读；返回主任务可继续对话。'
    : task && !terminalStatuses.has(task.status) ? '任务正在运行，结束后可继续对话。'
      : task ? '下一条消息会沿用这项任务的对话与工具记录。' : 'Agent 将在当前工作区中执行任务。';
  updateComposer();
  renderTaskList();
}

function updateComposer() {
  const task = currentTask();
  const allowed = !view.selected || (task && !task.parent_task_id && terminalStatuses.has(task.status));
  byId('prompt').disabled = !allowed;
  byId('prompt').placeholder = task ? '继续这项任务…' : '描述你希望 Agent 完成的工作…';
  byId('send-task').replaceChildren(task ? '继续对话 ' : '开始任务 ', element('span', '', '↗'));
  byId('send-task').disabled = view.busy || !view.ready || !allowed || !byId('prompt').value.trim();
}

function addEvent(event) {
  if (!event || !Number.isInteger(event.seq)) return;
  if (view.events.some((existing) => existing.seq === event.seq)) return;
  const scroll = byId('content-scroll');
  const follow = scroll.scrollHeight - scroll.scrollTop - scroll.clientHeight < 100;
  view.events.push(event);
  view.events.sort((a, b) => a.seq - b.seq);
  renderEvents();
  if (follow) scroll.scrollTop = scroll.scrollHeight;
  if (event.type === 'turn/end') refreshSelectedStatus();
}

async function loadTasks() {
  const result = await api('/agent/tasks?limit=50');
  view.tasks = result.tasks || [];
  renderStatus();
}

async function refreshSelectedStatus() {
  if (!view.selected) return;
  const taskId = view.selected;
  try {
    const task = await api(`/agent/tasks/${encodeURIComponent(taskId)}`);
    if (view.selected !== taskId) return;
    const index = view.tasks.findIndex((item) => item.task_id === taskId);
    if (index >= 0) view.tasks[index] = task;
    else view.tasks.unshift(task);
    renderStatus();
    if (terminalStatuses.has(task.status)) {
      closeStream();
      const history = await api(`/agent/tasks/${encodeURIComponent(taskId)}/events`);
      if (view.selected !== taskId) return;
      view.events = (history.events || []).sort((a, b) => a.seq - b.seq);
      renderEvents();
      await loadTasks();
    }
  } catch (error) {
    view.error = error.message;
    renderEvents();
  }
}

function startStream(taskId) {
  closeStream();
  const since = view.events.at(-1)?.seq ?? -1;
  const source = http.events(`/agent/tasks/${encodeURIComponent(taskId)}/stream?since=${since}`);
  view.stream = source;
  source.addEventListener('session/event', (message) => {
    if (view.selected !== taskId) return;
    try { addEvent(JSON.parse(message.data)); }
    catch { view.error = '收到无效的事件数据'; renderEvents(); }
  });
  source.onerror = () => {
    if (view.selected !== taskId) return;
    byId('connection-text').textContent = '重新连接中';
    refreshSelectedStatus();
  };
  source.onopen = () => {
    if (view.selected === taskId) byId('connection-text').textContent = '已连接';
  };
}

async function selectTask(taskId) {
  closeStream();
  view.selected = taskId;
  view.events = [];
  view.error = '';
  byId('sidebar').classList.remove('open');
  renderStatus();
  renderEvents();
  if (!taskId) { byId('prompt').focus(); return; }
  try {
    const [detail, history] = await Promise.all([
      api(`/agent/tasks/${encodeURIComponent(taskId)}`),
      api(`/agent/tasks/${encodeURIComponent(taskId)}/events`),
    ]);
    if (view.selected !== taskId) return;
    const index = view.tasks.findIndex((task) => task.task_id === taskId);
    if (index >= 0) view.tasks[index] = detail;
    else view.tasks.unshift(detail);
    view.events = (history.events || []).sort((a, b) => a.seq - b.seq);
    renderStatus();
    renderEvents();
    byId('content-scroll').scrollTop = byId('content-scroll').scrollHeight;
    if (!terminalStatuses.has(detail.status)) startStream(taskId);
  } catch (error) {
    if (view.selected !== taskId) return;
    view.error = error.message;
    renderEvents();
  }
}

function makeMessage(role, text, time) {
  const row = element('div', `event-row ${role}`);
  row.append(element('div', 'event-avatar', role === 'user' ? '你' : '✦'));
  const body = element('div', 'event-body');
  const author = element('div', 'event-author', role === 'user' ? '你' : 'Agent');
  author.append(element('span', 'event-time', clock(time)));
  body.append(author, element('div', 'event-text', text));
  row.append(body);
  return row;
}

function makeTool(call, result) {
  const details = element('details', 'tool-card');
  const summary = element('summary');
  summary.append(element('span', 'tool-glyph', '⚙'));
  summary.append(element('span', 'tool-name', call.data?.name || '工具'));
  const message = messageOf(result);
  const failed = Boolean(message?.isError || result?.data?.error);
  const duration = result ? `${Math.max(0, result.time - call.time)} ms` : '';
  summary.append(element('span', `tool-state${failed ? ' error' : ''}`, result ? `${failed ? '失败' : '完成'} · ${duration}` : '执行中'));
  summary.append(element('span', 'tool-chevron', '›'));
  details.append(summary);
  const content = element('div', 'tool-details');
  content.append(element('strong', '', '参数'), element('pre', '', preview(pretty(call.data?.arguments))));
  if (result) {
    content.append(element('strong', '', '结果'), element('pre', '', preview(textBlocks(message?.content) || pretty(result.data?.meta?.result))));
    const childId = result.data?.meta?.result?.child_task_id;
    if (childId) {
      const link = element('button', '', '查看子代理步骤');
      link.type = 'button';
      link.addEventListener('click', () => selectTask(childId));
      content.append(link);
    }
  }
  details.append(content);
  return details;
}

function reasonText(reason) {
  if (!reason) return '';
  if (reason.kind === 'completed') return '任务已完成';
  if (reason.kind === 'aborted') return '任务已停止';
  if (reason.kind === 'error') return reason.error?.message || '任务失败';
  return reason.kind || '';
}

function renderChat(container) {
  const list = element('div', 'event-list');
  const results = new Map();
  for (const event of view.events) {
    if (event.type === 'tool/result') {
      const id = messageOf(event)?.source?.callId || messageOf(event)?.toolCallId;
      if (id) results.set(id, event);
    }
  }
  for (const event of view.events) {
    if (event.type === 'turn/start' && event.data?.turn > 1) {
      list.append(element('div', 'step-divider', `第 ${event.data.turn} 轮对话`));
    } else if (event.type === 'user/message') {
      const text = textBlocks(messageOf(event)?.content);
      if (text) list.append(makeMessage('user', text, event.time));
    } else if (event.type === 'step/start') {
      list.append(element('div', 'step-divider', `第 ${event.data?.turn || 1} 轮 · 步骤 ${event.data?.step || ''}`));
    } else if (event.type === 'assistant/message') {
      const text = textBlocks(messageOf(event)?.content);
      if (text) list.append(makeMessage('assistant', text, event.time));
    } else if (event.type === 'tool/call') {
      list.append(makeTool(event, results.get(event.data?.callId)));
    } else if (event.type === 'subagent/start') {
      const childId = event.data?.child_task_id;
      if (childId) {
        const link = element('button', '', '子代理已启动 · 查看步骤');
        link.type = 'button';
        link.addEventListener('click', () => selectTask(childId));
        list.append(link);
      }
    } else if (event.type === 'turn/end') {
      const reason = event.data?.reason;
      list.append(element('div', `terminal-banner${reason?.kind === 'error' ? ' failed' : ''}`, reasonText(reason)));
    }
  }
  if (!list.childElementCount) list.append(element('div', 'history-empty', '等待任务事件…'));
  container.append(list);
}

function trajectoryLabel(event) {
  const data = event.data || {};
  switch (event.type) {
    case 'turn/start': return [`第 ${data.turn || 1} 轮开始`, 'Agent 已接收消息'];
    case 'user/message': return ['用户消息', textBlocks(messageOf(event)?.content)];
    case 'step/start': return [`第 ${data.turn || 1} 轮 · 步骤 ${data.step} 开始`, '正在请求模型'];
    case 'assistant/message': return ['模型回复', textBlocks(messageOf(event)?.content) || '请求调用工具'];
    case 'tool/call': return [`调用 ${data.name}`, pretty(data.arguments)];
    case 'tool/result': return ['工具结果', textBlocks(messageOf(event)?.content)];
    case 'subagent/start': return ['子代理开始', data.prompt || ''];
    case 'subagent/end': return ['子代理结束', data.status || ''];
    case 'step/end': return [`第 ${data.turn || 1} 轮 · 步骤 ${data.step} 结束`, ''];
    case 'turn/end': return [`第 ${data.turn || 1} 轮结束`, reasonText(data.reason)];
    default: return [event.type, pretty(data)];
  }
}

function renderTrajectory(container) {
  const list = element('div', 'trajectory');
  list.append(element('h2', 'trajectory-title', '任务轨迹'));
  const calls = new Map();
  for (const event of view.events) if (event.type === 'tool/call') calls.set(event.data?.callId, event);
  for (const event of view.events) {
    const [title, description] = trajectoryLabel(event);
    const isTool = event.type.startsWith('tool/');
    const isFailed = event.type === 'turn/end' && event.data?.reason?.kind === 'error';
    const row = element('div', `trajectory-row${isTool ? ' tool' : ''}${isFailed ? ' failed' : ''}`);
    row.append(element('span', 'trajectory-time', clock(event.time)));
    const rail = element('span', 'trajectory-rail');
    rail.append(element('span', 'trajectory-dot'));
    row.append(rail);
    const summary = element('div', 'trajectory-summary');
    summary.append(element('strong', '', title));
    if (description) summary.append(element('small', '', description.replace(/\s+/g, ' ').slice(0, 220)));
    if (event.type === 'subagent/start' && event.data?.child_task_id) {
      const link = element('button', '', '查看子代理步骤');
      link.type = 'button';
      link.addEventListener('click', () => selectTask(event.data.child_task_id));
      summary.append(link);
    }
    if (event.type === 'tool/result') {
      const source = messageOf(event)?.source;
      const call = calls.get(source?.callId);
      if (call) {
        const detail = element('details', 'trajectory-detail');
        detail.append(element('summary', '', '查看输入与输出'));
        detail.append(element('pre', '', preview(`输入\n${pretty(call.data?.arguments)}\n\n输出\n${description}`)));
        summary.append(detail);
      }
      const childId = event.data?.meta?.result?.child_task_id;
      if (childId) {
        const link = element('button', '', '查看子代理步骤');
        link.type = 'button';
        link.addEventListener('click', () => selectTask(childId));
        summary.append(link);
      }
    }
    row.append(summary);
    const duration = event.type === 'tool/result' ? event.data?.meta?.durationMs : null;
    row.append(element('span', 'trajectory-duration', typeof duration === 'number' ? `${duration} ms` : ''));
    list.append(row);
  }
  if (view.events.length === 0) list.append(element('div', 'history-empty', '等待任务事件…'));
  container.append(list);
}

function renderEvents() {
  const container = byId('content');
  container.replaceChildren();
  if (view.error) container.append(element('div', 'error-banner', view.error));
  if (!view.selected) {
    container.append(welcome);
    return;
  }
  runtime.entries('task.view').find((entry) => entry.id === view.tab)?.render(container);
}

async function submitTask(event) {
  event.preventDefault();
  const prompt = byId('prompt').value.trim();
  const task = currentTask();
  if (!prompt || !view.ready || view.busy || (view.selected && (!task || task.parent_task_id || !terminalStatuses.has(task.status)))) return;
  view.busy = true;
  updateComposer();
  view.error = '';
  try {
    const created = await api(task ? `/agent/tasks/${encodeURIComponent(task.task_id)}/messages` : '/agent/tasks', {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ prompt }),
    });
    byId('prompt').value = '';
    await loadTasks();
    await selectTask(created.task_id);
  } catch (error) {
    view.error = error.message;
    renderEvents();
  } finally {
    view.busy = false;
    updateComposer();
  }
}

async function cancelTask() {
  if (!view.selected) return;
  byId('cancel-task').disabled = true;
  try {
    await api(`/agent/tasks/${encodeURIComponent(view.selected)}`, { method: 'DELETE' });
    byId('task-status').textContent = '正在停止';
  } catch (error) {
    view.error = error.message;
    renderEvents();
  } finally {
    byId('cancel-task').disabled = false;
  }
}

async function initialize(ctx) {
  ctx.effect(closeStream);
  const storedTheme = localStorage.getItem('agent-theme');
  if (storedTheme === 'dark' || (!storedTheme && matchMedia('(prefers-color-scheme: dark)').matches)) {
    document.body.setAttribute('data-ds-dark-theme', '');
  }
  ctx.listen(byId('theme-toggle'), 'click', () => {
    const dark = document.body.toggleAttribute('data-ds-dark-theme');
    localStorage.setItem('agent-theme', dark ? 'dark' : 'light');
  });
  ctx.listen(byId('new-task'), 'click', () => selectTask(null));
  for (const entry of runtime.entries('task.view')) {
    ctx.listen(byId(entry.button), 'click', () => setTab(entry.id));
  }
  ctx.listen(byId('task-form'), 'submit', submitTask);
  ctx.listen(byId('cancel-task'), 'click', cancelTask);
  ctx.listen(byId('parent-task'), 'click', () => {
    const parentId = currentTask()?.parent_task_id;
    if (parentId) selectTask(parentId);
  });
  ctx.listen(byId('menu-toggle'), 'click', () => byId('sidebar').classList.toggle('open'));
  ctx.listen(byId('prompt'), 'input', updateComposer);
  ctx.listen(byId('prompt'), 'keydown', (event) => {
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      byId('task-form').requestSubmit();
    }
  });
  try {
    const [info] = await Promise.all([api('/agent/info'), loadTasks()]);
    view.ready = Boolean(info.ready);
    byId('workspace-path').textContent = info.workspace;
    byId('workspace-path').title = info.workspace;
    byId('connection').classList.add(view.ready ? 'ready' : 'error');
    byId('connection-text').textContent = view.ready ? `就绪 · ${info.model}` : '请先配置模型';
    if (!view.ready) view.error = '尚未配置主代理模型。请打开左侧「设置」添加提供商与模型。';
    updateComposer();
    if (view.tasks.length) await selectTask(view.tasks[0].task_id);
    else renderEvents();
  } catch (error) {
    view.error = error.message;
    byId('connection').classList.add('error');
    byId('connection-text').textContent = '服务未就绪';
    renderEvents();
  }
  ctx.interval(() => { if (currentTask()?.status === 'running') refreshSelectedStatus(); }, 2500);
}

async function boot() {
  runtime = new BrowserRuntime();
  await runtime.mount(httpPlugin);
  await runtime.mount({
    id: 'chat-view',
    start(ctx) { ctx.contribute('task.view', { id: 'chat', button: 'tab-chat', render: renderChat }); },
  });
  await runtime.mount({
    id: 'trajectory-view',
    start(ctx) { ctx.contribute('task.view', { id: 'trajectory', button: 'tab-trajectory', render: renderTrajectory }); },
  });
  await runtime.mount({
    id: 'task-shell', requires: ['http'],
    async start(ctx) { http = ctx.get('http'); await initialize(ctx); },
  });
  window.addEventListener('pagehide', () => runtime.dispose(), { once: true });
}

boot().catch((error) => {
  byId('connection').classList.add('error');
  byId('connection-text').textContent = error.message;
  console.error('浏览器插件启动失败', error);
});
