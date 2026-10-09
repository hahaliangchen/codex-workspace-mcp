import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { flowState, chatItems, trajectoryRows } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const event = (type, data) => ({ type, time: 0, data })

test('ended runs retain the Organizer assessment without claiming goal completion', () => {
  const events = [event('turn/start', { turn: 1 }),
    event('organizer/decision', { turn: 1, decision: { action: 'blocked', achieved: false, unresolved: ['视觉验收未完成'] } }),
    event('turn/end', { turn: 1, reason: { kind: 'completed' } })]
  const reason = chatItems(JSON.parse(JSON.stringify(events))).find(item => item.kind === 'turn-end').reason
  assert.equal(reason.kind, 'completed')
  assert.equal(reason.goalAchieved, false)
  assert.deepEqual(reason.unresolved, ['视觉验收未完成'])
  const next = [...events, event('turn/start', { turn: 2 }),
    event('turn/end', { turn: 2, reason: { kind: 'completed', goal_achieved: true, unresolved: [] } })]
  assert.equal(chatItems(next).filter(item => item.kind === 'turn-end')[1].reason.goalAchieved, true)
  const unknown = [...events, event('turn/start', { turn: 3 }), event('user/message', { message: { content: '下一项任务' } })]
  assert.equal(chatItems(unknown, { status: 'completed', task_id: 'task' }).at(-1).reason.goalAchieved, undefined,
    'a previous turn assessment does not establish completion of the new goal')
})

test('Observer retrospective is visible after the answer and survives history reload', () => {
  const events = [event('turn/start', { turn: 1 }),
    event('user/message', { message: { content: [{ type: 'text', text: '运行示例' }] } }),
    event('assistant/message', { turn: 1, message: { content: [{ type: 'text', text: '已加载示例' }] } }),
    event('turn/end', { turn: 1, reason: { kind: 'completed' } }),
    event('observer/retrospective', { status: 'completed', summary: 'short host projection',
      observer_return: { summary: '加载成功，视觉失败已说明。', path_review: '重复探测没有增加证据。',
        shortening_opportunities: ['复用已有服务。'] } })]
  const task = { task_id: 'task', status: 'completed', created_at: 0, updated_at: 1 }
  const items = chatItems(JSON.parse(JSON.stringify(events)), task)
  const review = items.find(item => item.kind === 'retrospective')
  assert.match(review.text, /加载成功，视觉失败已说明/)
  assert.match(review.text, /重复探测/)
  assert.match(review.text, /复用已有服务/)
  assert.ok(items.indexOf(review) > items.findIndex(item => item.kind === 'assistant'))
  assert.equal(items.filter(item => item.kind === 'turn-end').length, 1)
})

test('six task stages retain their tools and failures after a completed history is reloaded', () => {
  const goals = ['服务探测', '打开浏览器', '上传并等待加载', '读取初始页码', '截图与翻页核对', '浏览器诊断']
  const tools = ['http_probe', 'browser_open', 'browser_upload', 'browser_read', 'browser_screenshot', 'browser_diagnostics']
  const events = [event('turn/start', { turn: 1 })]
  const nodes = []
  for (let i = 0; i < goals.length; i++) {
    nodes.push({ id: `task_${i}`, node_id: `work_task_${i}`, kind: 'worker', title: goals[i], request_id: 1, revision: 1, plan_revision: 1, status: 'running' })
    events.push(event('flow/plan', { turn: 1, mode: 'dag', nodes: structuredClone(nodes), active_node_id: `task_${i}` }))
    events.push(event('tool/call', { turn: 1, flowNodeId: `work_task_${i}`, callId: `call_${i}`, name: tools[i], arguments: '{}' }))
    events.push(event('tool/result', { turn: 1, toolCallId: `call_${i}`, message: { toolCallId: `call_${i}`, isError: i === 5, content: [{ type: 'text', text: i === 5 ? 'original diagnostic error' : 'actual tool result' }] } }))
    nodes[i].status = 'done'
  }
  events.push(event('flow/plan', { turn: 1, mode: 'dag', nodes, active_node_id: '' }))
  events.push(event('turn/end', { turn: 1, reason: { kind: 'completed' } }))
  const reloaded = flowState(JSON.parse(JSON.stringify(events)))
  assert.equal(reloaded.nodes.length, 6, 'tool routing must not create extra Worker nodes')
  assert.deepEqual(reloaded.nodes.map(n => n.title), goals)
  assert.deepEqual(reloaded.nodes.map(n => n.tools[0].name), tools)
  assert.equal(reloaded.nodes.every(n => n.status === 'done'), true)
  assert.equal(reloaded.nodes[5].tools[0].failed, true)
  assert.equal(reloaded.nodes[5].tools[0].output, 'original diagnostic error')
  assert.equal(reloaded.activeNodeId, undefined)
  const chat = chatItems(JSON.parse(JSON.stringify(events)))
  assert.equal(chat.filter(item => item.kind === 'tool').length, 6)
  assert.equal(chat.find(item => item.kind === 'tool' && item.callId === 'call_5').result.isError, true)
})


test('ordinary work messages and visual input limitations survive history reload', () => {
  const limitation = '模型图像能力未知，未配置备用视觉服务；截图已保存，本次没有收到图像输入。'
  const events = [event('turn/start', { turn: 1 }),
    event('assistant/message', { turn: 1, step: 1, message: { content: '已经加载完成，继续读取页码。' } }),
    event('visual/input_result', { turn: 1, actor: 'worker', request_trace_id: 'visual-1', message: { role: 'system', content: limitation } }),
    event('turn/end', { turn: 1, reason: { kind: 'completed' } })]
  const restored = JSON.parse(JSON.stringify(events))
  assert.equal(chatItems(restored).filter(item => item.kind === 'assistant').length, 1)
  const row = trajectoryRows(restored).find(item => item.type === 'visual/input_result')
  assert.equal(row.detail, limitation)
  assert.equal(row.name, '视觉输入结果')
})
