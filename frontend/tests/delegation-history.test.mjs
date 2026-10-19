import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { chatItems, flowState } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const e = (seq, type, data) => ({seq,time:1000+seq,type,data:{turn:1,...data}})
const child = (seq,id,type,data) => e(seq,'subagent/event',{agent_id:id,event_type:type,event:{agent_id:id,turn:1,step:1,...data}})

test('dispatch order stays fixed while reverse completion and interleaved tool calls remain scoped', () => {
  const events = [
    e(0,'user/message',{message:{content:'parallel work'}}),
    e(1,'subagent/start',{agent_id:'slow',name:'First task'}),
    e(2,'subagent/start',{agent_id:'fast',name:'Second task'}),
    child(3,'slow','tool/call',{callId:'same-call',name:'read_file',arguments:'{"path":"slow"}'}),
    child(4,'fast','tool/call',{callId:'same-call',name:'read_file',arguments:'{"path":"fast"}'}),
    child(5,'fast','tool/result',{message:{toolCallId:'same-call',content:[{type:'text',text:'FAST_DATA'}]}}),
    child(6,'fast','assistant/message',{message:{content:'FAST_REPORT'}}),
    e(7,'subagent/end',{agent_id:'fast',status:'completed',result:{report:'FAST_REPORT'}}),
    e(8,'assistant/message',{step:2,message:{content:'Main continues independent work'}}),
    child(9,'slow','tool/result',{message:{toolCallId:'same-call',content:[{type:'text',text:'SLOW_DATA'}]}}),
    e(10,'subagent/review',{agent_id:'fast',review:{decision:'accepted',evidence:'Main checked actual file'}}),
    e(11,'subagent/end',{agent_id:'slow',status:'failed',result:{error:'specific failure'}}),
  ]
  const rows=chatItems([...events].reverse())
  assert.deepEqual(rows.map(r=>r.kind),['user','subagent','subagent','assistant'])
  const groups=rows.filter(r=>r.kind==='subagent')
  assert.deepEqual(groups.map(r=>r.agentId),['slow','fast'])
  assert.equal(chatItems(groups[0].events)[0].result.text,'SLOW_DATA')
  assert.equal(chatItems(groups[1].events)[0].result.text,'FAST_DATA')
  assert.equal(groups[0].status,'failed')
  assert.equal(groups[0].report,'specific failure')
  assert.match(groups[1].review,/accepted/)
  assert.equal(rows.at(-1).text,'Main continues independent work')
  assert.deepEqual(chatItems(JSON.parse(JSON.stringify(events))),rows)
})

test('flow tool results remain attached to the actual delegate request when call IDs collide', () => {
  const events=[e(0,'flow/node_add',{node:{id:'parent',kind:'agent',status:'done'}}),
    e(1,'flow/node_add',{node:{id:'a',kind:'worker',status:'running',parent_id:'parent'}}),
    e(2,'flow/node_add',{node:{id:'b',kind:'worker',status:'running',parent_id:'parent'}}),
    child(3,'slow','tool/call',{flowNodeId:'a',callId:'same',name:'read_file',arguments:'{}'}),
    child(4,'fast','tool/call',{flowNodeId:'b',callId:'same',name:'read_file',arguments:'{}'}),
    child(5,'slow','tool/result',{flowNodeId:'a',message:{toolCallId:'same',content:[{type:'text',text:'A'}]}}),
    child(6,'fast','tool/result',{flowNodeId:'b',message:{toolCallId:'same',content:[{type:'text',text:'B'}]}})]
  const flow=flowState(events)
  assert.equal(flow.nodes.find(n=>n.id==='turn_1:a').tools[0].output,'A')
  assert.equal(flow.nodes.find(n=>n.id==='turn_1:b').tools[0].output,'B')
})
