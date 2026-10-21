import assert from 'node:assert/strict'
import {readFile} from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/sidebarModel.ts', import.meta.url),'utf8')
const {outputText} = ts.transpileModule(source,{compilerOptions:{module:ts.ModuleKind.ESNext,target:ts.ScriptTarget.ES2024}})
const {isChat,taskTitle,visibleTasks,projectGroups} = await import('data:text/javascript;base64,'+Buffer.from(outputText).toString('base64'))
const task = (id, extra={}) => ({task_id:id,prompt:id,status:'completed',updated_at:1,parent_task_id:null,...extra})
test('chat and code histories are disjoint, blank drafts stay out, titles are searchable', () => {
  const tasks=[task('code',{title:'Renamed'}),task('plugin',{plugin_id:'ppt-master'}),task('chat',{kind:'chat'}),task('blank',{prompt:'',status:'draft'}),task('archived',{archived:true})]
  const shown=visibleTasks(tasks,'',false)
  assert.deepEqual(shown.filter(isChat).map(t=>t.task_id),['chat','plugin'])
  assert.deepEqual(shown.filter(t=>!isChat(t)).map(t=>t.task_id),['code'])
  assert.equal(taskTitle(tasks[0]),'Renamed')
  assert.deepEqual(visibleTasks(tasks,'renamed',false).map(t=>t.task_id),['code'])
  assert.deepEqual(visibleTasks(tasks,'',true).map(t=>t.task_id),['archived'])
})
test('projects keep registered order, dedupe paths, and assign sessions to their own directory', () => {
  const projects=[{path:'/a/project',title:'project'},{path:'/b/project',title:'project'},{path:'/a/project',title:'project'}]
  const {groups,ungrouped}=projectGroups(projects,[task('a',{workspace:'/a/project'}),task('b',{workspace:'/b/project'}),task('removed',{workspace:'/removed'}),task('plugin',{plugin_id:'ppt-master'})],'/a/project')
  assert.deepEqual(groups.map(p=>p.path),['/a/project','/b/project'])
  assert.deepEqual(groups.map(p=>p.tasks.map(t=>t.task_id)),[['a'],['b']])
  assert.deepEqual(ungrouped.map(t=>t.task_id),['removed'])
})
test('only explicit pin changes ordering and recency is stable for equal timestamps', () => {
  assert.deepEqual(visibleTasks([task('recent',{updated_at:10}),task('pinned',{pinned:true}),task('b'),task('a')],'',false).map(t=>t.task_id),['pinned','recent','a','b'])
})
