import assert from 'node:assert/strict'
import { webcrypto } from 'node:crypto'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/randomId.ts', import.meta.url), 'utf8')
const { outputText } = ts.transpileModule(source, {compilerOptions: {module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2024}})
const { randomId } = await import('data:text/javascript;base64,' + Buffer.from(outputText).toString('base64'))

test('HTTP context generates unique RFC 4122 v4 request IDs without randomUUID', () => {
  const insecureContext = {getRandomValues: webcrypto.getRandomValues.bind(webcrypto)}
  const ids = Array.from({length: 1000}, () => randomId(insecureContext))
  assert.equal(new Set(ids).size, ids.length)
  for (const id of ids) assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/)
})
test('HTTPS context keeps the native UUID implementation bound to its Crypto object', () => {
  const crypto = {randomUUID() {assert.equal(this, crypto); return 'native-uuid'}, getRandomValues() {throw Error('unexpected fallback')}}
  assert.equal(randomId(crypto), 'native-uuid')
})
