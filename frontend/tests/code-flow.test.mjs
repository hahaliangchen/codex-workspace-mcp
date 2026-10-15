import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'
const source = await readFile(new URL('../src/builtin_plugins/code-flow/graph.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024 } })
const { layoutCodeFlow } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))

test('branches converge on one shared node and split downstream', () => {
  const positions = layoutCodeFlow(['a', 'b', 'shared', 'c', 'd'], [
    { source: 'a', target: 'shared' }, { source: 'b', target: 'shared' },
    { source: 'shared', target: 'c' }, { source: 'shared', target: 'd' },
  ])
  assert.equal(positions.size, 5)
  assert.equal(positions.get('a').y, positions.get('b').y)
  assert.ok(positions.get('shared').y > positions.get('a').y)
  assert.ok(positions.get('c').y > positions.get('shared').y)
  assert.equal(positions.get('c').y, positions.get('d').y)
})
test('cycles and disconnected nodes remain present with finite positions', () => {
  const positions = layoutCodeFlow(['a', 'b', 'tail', 'alone'], [
    { source: 'a', target: 'b' }, { source: 'b', target: 'a' }, { source: 'b', target: 'tail' },
    { source: 'unknown', target: 'a' },
  ])
  assert.equal(positions.size, 4)
  assert.equal(positions.get('a').y, positions.get('b').y)
  assert.ok(positions.get('tail').y > positions.get('b').y)
  assert.equal(new Set([...positions.values()].map(p => `${p.x},${p.y}`)).size, 4)
  assert.ok([...positions.values()].every(p => Number.isFinite(p.x) && Number.isFinite(p.y)))
})
