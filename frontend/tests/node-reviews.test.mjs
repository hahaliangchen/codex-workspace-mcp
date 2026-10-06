import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/model.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { flowState, reviewMatchesNode } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))
const event = (type, data) => ({ type, time: 0, data })
const identity = (request_id, revision = 1, plan_revision = 1) => ({ request_id, work_id: 'same', node_id: 'n', revision, plan_revision })
const review = (id, identity, status, turn = 1) => event('observer/node_review', { review_id: id, identity, status, turn, stage: 'handoff', result: { assessment: status === 'completed' ? 'uncertain' : undefined, summary: 'original version' } })
const plan = (turn, request_id, revision, plan_revision) => [event('turn/start', { turn }), event('flow/plan', { turn, active_node_id: 'same', nodes: [{ id: 'same', node_id: 'n', request_id, revision, plan_revision, status: 'running' }] }), event('scheduler/state', { turn, state: { request_started_turn: request_id, current: 'same', frames: { same: { status: 'running', order: { id: 'same', node_id: 'n', revision, plan_revision, goal: 'new work' } } } } })]

test('review status updates deduplicate without creating execution nodes or changing highlight', () => {
  const state = flowState([...plan(1, 1, 1, 1), review('a', identity(1), 'pending'), review('a', identity(1), 'reviewing'), review('a', identity(1), 'unassessed')])
  assert.equal(state.nodeReviews.length, 1)
  assert.equal(state.nodeReviews[0].status, 'unassessed')
  assert.equal(state.nodes.length, 1)
  assert.equal(state.nodes[0].status, 'running')
  assert.equal(state.activeNodeId, 'turn_1:same')
})
test('late reused-node review belongs only to its original request and revision', () => {
  const state = flowState([...plan(1, 1, 1, 1), review('old', identity(1), 'pending'), ...plan(2, 2, 2, 3), review('old', identity(1), 'completed')])
  assert.equal(reviewMatchesNode(state.nodeReviews[0], state.nodes.find(n => n.id === 'turn_1:same')), true)
  assert.equal(reviewMatchesNode(state.nodeReviews[0], state.nodes.find(n => n.id === 'turn_2:same')), false)
  assert.equal(state.activeNodeId, 'turn_2:same')
})
test('Organizer response remains attached to the source review with implemented application', () => {
  const state = flowState([...plan(1, 1, 1, 1), review('a', identity(1), 'completed'), event('observer/advice_response', { turn: 1, actor: 'organizer', advice: { review_id: 'a', decision_id: 'decision_b', disposition: 'accepted', application: { work_id: 'b', field: 'constraints', value: ['Reuse A'] } } })])
  assert.equal(state.nodeReviews[0].responses[0].decision_id, 'decision_b')
  assert.equal(state.nodeReviews[0].responses[0].application.work_id, 'b')
})

test('changed handoff advice has its own response while the assignment handling stays historical', () => {
  for (const disposition of ['accepted', 'resolved', 'declined']) {
    const state = flowState([...plan(1, 1, 1, 1),
      event('observer/node_review', { review_id: 'assignment', identity: identity(1), status: 'completed', stage: 'assignment', source_event_id: 'assigned', result: { summary: 'reuse inputs' } }),
      event('observer/advice_response', { actor: 'organizer', advice: { id: 'advice_1', review_id: 'assignment', disposition, decision_id: 'before_delivery' } }),
      event('observer/node_review', { review_id: 'handoff', identity: identity(1), status: 'completed', stage: 'handoff', source_event_id: 'sealed', result: { summary: 'new upstream defect' } }),
      event('observer/advice_delivered', { advice: { id: 'advice_2', review_id: 'handoff', disposition: 'unread', supersedes_advice_id: 'advice_1' } }),
      event('observer/advice_response', { actor: 'organizer', advice: { id: 'advice_2', review_id: 'handoff', disposition: 'adjusted', decision_id: 'after_delivery', application: { field: 'revisit', value: 'upstream' } } }),
    ])
    const [assignment, handoff] = state.nodeReviews
    assert.equal(assignment.stage, 'assignment')
    assert.equal(assignment.sourceEventId, 'assigned')
    assert.equal(assignment.responses.length, 1)
    assert.equal(assignment.responses[0].disposition, disposition)
    assert.equal(handoff.stage, 'handoff')
    assert.equal(handoff.sourceEventId, 'sealed')
    assert.equal(handoff.responses.length, 1)
    assert.equal(handoff.responses[0].decision_id, 'after_delivery')
    assert.equal(handoff.responses[0].application.field, 'revisit')
    assert.equal(state.nodes.length, 1)
    assert.equal(state.activeNodeId, 'turn_1:same')
  }
})

test('A to B to A reviews retain separate provenance and Organizer responses', () => {
  const events = [...plan(1, 1, 1, 1)]
  for (const [index, summary] of ['A', 'B', 'A'].entries()) {
    const id = `review_${index + 1}`
    events.push(event('observer/node_review', { review_id: id, identity: identity(1), status: 'completed', stage: 'handoff', source_event_id: `source_${index + 1}`, result: { summary } }))
    events.push(event('observer/advice_delivered', { advice: { id: `advice_${index + 1}`, review_id: id, disposition: 'unread', supersedes_advice_id: index ? `advice_${index}` : null } }))
    events.push(event('observer/advice_response', { actor: 'organizer', advice: { id: `advice_${index + 1}`, review_id: id, disposition: index === 2 ? 'accepted' : 'resolved', decision_id: `decision_${index + 1}` } }))
  }
  const state = flowState(events)
  assert.equal(state.nodeReviews.length, 3)
  for (const [index, item] of state.nodeReviews.entries()) {
    assert.equal(item.sourceEventId, `source_${index + 1}`)
    assert.equal(item.responses.length, 1)
    assert.equal(item.responses[0].decision_id, `decision_${index + 1}`)
  }
  assert.equal(state.nodeReviews[0].responses[0].disposition, 'resolved')
  assert.equal(state.nodeReviews[2].responses[0].disposition, 'accepted')
  assert.equal(state.nodes.length, 1)
  assert.equal(state.activeNodeId, 'turn_1:same')
})
