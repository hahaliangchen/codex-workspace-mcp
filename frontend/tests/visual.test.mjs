import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/visual.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { visualRecords, foldImageData } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const identity = (request_id, revision = 1, plan_revision = 1) => ({ task_id: 't', request_id, work_id: 'w', node_id: 'n', revision, plan_revision })
const artifact = (id, identity) => ({ artifact_id: id, identity, task_id: 't', width: 1280, height: 800, content_hash: id })
const node = { id: 'turn_2:w', requestId: 2, revision: 2, planRevision: 3, workUnit: { id: 'w' } }
test('late pictures and judgments stay on their original request and instance', () => {
  const old = artifact('old', identity(1))
  const current = artifact('current', identity(2, 2, 3))
  const events = [
    { type: 'tool/result', data: { meta: { result: { visual_artifact: current } } } },
    { type: 'observer/node_review', data: { result: { visual_artifacts: [old] } } },
    { type: 'visual/check_result', data: { result: { identity: old.identity, assessment: 'pass', artifacts: [old] } } },
    { type: 'visual/check_result', data: { result: { identity: current.identity, assessment: 'issue', artifacts: [current] } } },
    { type: 'visual/model_received', data: { manifest: { identity: current.identity, status: 'direct', images: [current] } } },
  ]
  const records = visualRecords(events, node)
  assert.deepEqual(records.artifacts.map(a => a.artifact_id), ['current'])
  assert.equal(records.checks[0].assessment, 'issue')
  assert.equal(records.checks.length, 1)
  assert.equal(records.dispatches.length, 1)
  assert.deepEqual(visualRecords(events, { ...node, requestId: 1, revision: 1, planRevision: 1 }).artifacts.map(a => a.artifact_id), ['old'])
})
test('unknown dispatch records selected pictures without claiming they were sent', () => {
  const selected = artifact('saved', identity(2, 2, 3))
  const records = visualRecords([{ type: 'visual/model_received', data: { manifest: { identity: selected.identity, status: 'unknown_capability', images: [], selected_images: [selected] } } }], node)
  assert.equal(records.dispatches[0].images.length, 0)
  assert.equal(records.dispatches[0].status, 'unknown_capability')
})
test('explicitly consumed upstream pictures are previewed with their original owner', () => {
  const upstream = artifact('upstream', { ...identity(2, 1, 1), work_id: 'source', node_id: 'source' })
  assert.equal(visualRecords([{ type: 'tool/result', data: { meta: { result: { visual_artifact: upstream } } } }], node).artifacts.length, 0)
  const records=visualRecords([{ type: 'visual/model_received', data: { manifest: { identity: identity(2, 2, 3), status: 'direct', images: [upstream] } } }], node)
  assert.equal(records.artifacts[0].identity.work_id, 'source')
  assert.equal(records.artifacts[0].artifact_id, 'upstream')
})
test('request view folds image bytes without modifying the real request body', () => {
  const body = { messages: [{ role: 'user', content: [{ type: 'image_url', image_url: { url: 'data:image/png;base64,AAAA' } }] }] }
  assert.match(foldImageData(body).messages[0].content[0].image_url.url, /图片数据已折叠/)
  assert.equal(body.messages[0].content[0].image_url.url, 'data:image/png;base64,AAAA')
})
test('failed image requests retain selected pictures and the original provider reason after reload', () => {
  const picture = artifact('rejected', identity(2, 2, 3))
  const events = [{ type: 'visual/model_failed', data: { error: 'HTTP 400: image input unsupported', manifest: { identity: picture.identity, status: 'direct', images: [picture], actor: 'worker' } } }]
  const records = visualRecords(JSON.parse(JSON.stringify(events)), node)
  assert.equal(records.dispatches[0].status, 'request_failed')
  assert.equal(records.dispatches[0].dispatch_status, 'direct')
  assert.equal(records.dispatches[0].error, events[0].data.error)
  assert.equal(records.artifacts[0].artifact_id, 'rejected')
  assert.equal(records.checks.length, 0)
})
