import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { flowState, chatItems } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const event = (type, data) => ({ type, time: 0, data })

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
