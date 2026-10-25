import assert from 'node:assert/strict'
import test from 'node:test'
import { mkdir } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

test('host steps expose AI replies and tools, with finish last and execution-end wording', { skip: !process.env.PLAYWRIGHT_MODULE }, async () => {
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE)
  const output = fileURLToPath(new URL('../../target/context-acceptance/ui/', import.meta.url))
  await mkdir(output, { recursive: true })
  const server = await createServer({
    root: fileURLToPath(new URL('../', import.meta.url)), configFile: fileURLToPath(new URL('../vite.config.ts', import.meta.url)),
    server: { port: 0, host: '127.0.0.1', hmr: false },
    plugins: [{ name: 'host-step-test', configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (req.url !== '/agent/host-step-test') return next()
        const html = await server.transformIndexHtml(req.url, `<!doctype html><html><head><meta charset="utf-8">
<style>html,body,#root{height:100%;margin:0;font-family:Arial,sans-serif}*{box-sizing:border-box}</style></head><body><div id="root"></div><script type="module">
import React from 'react';import {createRoot} from 'react-dom/client';
import {FlowView} from '/agent/src/components/FlowView.tsx';import {CordisProvider} from '/agent/src/cordis/react.tsx';
const event=(seq,type,data)=>({seq,type,time:seq,data:{turn:1,...data}});
const node=(seq,id,parent,title,kind,status,ordinal,result)=>event(seq,'task/node_planned',{node_id:id,parent_id:parent,title,goal:title,kind,status,ordinal,result,executor:'inline'});
const events=[node(1,'main',null,'检查开关','task','running',0),node(2,'finish','main','结束','finish','pending',10000),
node(3,'round1','main','第一轮','step','current',0),event(4,'flow/node_add',{node:{id:'req1',kind:'agent',task_node_id:'round1',status:'done'}}),
event(5,'tool/call',{flowNodeId:'req1',callId:'read',name:'read_file',arguments:'{}'}),
event(6,'tool/result',{flowNodeId:'req1',message:{toolCallId:'read',content:[{type:'text',text:'开关触点正常'}]}}),
event(7,'assistant/draft',{message:{role:'assistant',content:'已检查开关。\\n触点正常，可以继续加热。'}}),
node(8,'round1','main','第一轮','step','done',0,{response:{event_seq:7}}),
node(9,'round2','main','收尾确认','step','done',1,{response:{event_seq:10}}),
event(10,'assistant/message',{message:{role:'assistant',content:'检查执行结束，结果交由用户查看。'}}),
node(11,'finish','main','结束','finish','done',10000,{result:'检查执行结束。'}),node(12,'main',null,'检查开关','task','done',0,{result:'检查执行结束。'}),
event(13,'turn/end',{reason:{kind:'completed'}})];
createRoot(document.getElementById('root')).render(React.createElement(CordisProvider,{context:{agentApi:{contextDebug:async()=>({enabled:false})}}},
React.createElement(FlowView,{events,task:{task_id:'host-step',status:'completed'},running:false})));
</script></body></html>`)
        res.setHeader('Content-Type', 'text/html'); res.end(html)
      })
    } }],
  })
  let browser
  try {
    await server.listen()
    browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_EXECUTABLE_PATH ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH } : {}) })
    const page = await browser.newPage()
    const errors = []
    page.on('pageerror', error => errors.push(String(error)))
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      await page.setViewportSize(viewport)
      await page.goto(server.resolvedUrls.local[0] + 'host-step-test')
      await page.waitForFunction(() => document.querySelectorAll('.react-flow__node').length === 4)
      try {
        await page.locator('[data-node-id="task:round1"]').waitFor({ state: 'visible', timeout: 10000 })
      } catch (error) {
        await page.screenshot({ path: output + 'host-step-failure.png' })
        throw error
      }
      const x = async id => (await page.locator('[data-node-id="task:' + id + '"]').boundingBox()).x
      assert.ok(await x('round1') < await x('round2'))
      assert.ok(await x('round2') < await x('finish'))
      await page.locator('[data-node-id="task:round1"]').click()
      const reply = page.getByText('本轮 AI 回复', { exact: true }).locator('../..')
      await reply.waitFor()
      assert.match(await reply.textContent(), /已检查开关。\s*触点正常，可以继续加热。/)
      assert.equal(await page.getByText('开关触点正常', { exact: true }).count(), 1)
      assert.ok(await page.getByText('任务已结束', { exact: true }).count() > 0)
      assert.equal(await page.getByText('任务已完成', { exact: true }).count(), 0)
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true)
      await page.getByTitle('关闭详情').locator('../..').evaluate(el => Promise.all(el.getAnimations().map(animation => animation.finished)))
      await page.screenshot({ path: output + 'host-step-' + viewport.width + '.png' })
    }
    assert.deepEqual(errors, [])
  } finally {
    if (browser) await browser.close()
    await server.close()
  }
})

test('task tree renders across viewports and request comparison stays in its node', { skip: !process.env.PLAYWRIGHT_MODULE }, async () => {
  const { chromium } = await import(process.env.PLAYWRIGHT_MODULE)
  const output = fileURLToPath(new URL('../../target/context-acceptance/ui/', import.meta.url))
  await mkdir(output, { recursive: true })
  const server = await createServer({
    root: fileURLToPath(new URL('../', import.meta.url)),
    configFile: fileURLToPath(new URL('../vite.config.ts', import.meta.url)),
    server: { port: 0, host: '127.0.0.1', hmr: false },
    plugins: [{ name: 'task-tree-test', configureServer(server) {
      server.middlewares.use(async (req, res, next) => {
        if (req.url !== '/agent/task-tree-test') return next()
        const html = await server.transformIndexHtml(req.url, `<!doctype html><html><head><meta charset="utf-8">
<style>html,body,#root{height:100%;margin:0;font-family:Arial,sans-serif}*{box-sizing:border-box}</style></head><body><div id="root"></div><script type="module">
import React from 'react';import {createRoot} from 'react-dom/client';
import {FlowView} from '/agent/src/components/FlowView.tsx';
import {CordisProvider} from '/agent/src/cordis/react.tsx';
const event=(type,data)=>({type,time:0,data:{turn:1,...data}});
const node=(id,parent,title,status,kind='planned')=>event('task/node_planned',{node_id:id,parent_id:parent,title,goal:title,status,kind,executor:'inline'});
const events=[node('n0',null,'烧水热菜','suspended','root'),node('page7','n0','打开开关','suspended'),
node('repair','page7','修理电源开关并重新验证','running','branch'),node('page8','n0','放上托盘和菜','pending'),
node('decision','n0','确认替换零件','needs_user'),event('turn/end',{turn:2,reason:{kind:'completed'}})];
const items=[{id:3,turn:2,task_node_id:'repair'},{id:2,turn:2,task_node_id:'page8'},{id:1,turn:1,task_node_id:'repair'}]
.map(item=>({...item,nodeId:'request'+item.id,actor:'agent',step:item.id,status:'completed',tools:[],stage:'execution',model:'test',message_count:1,request_bytes:100}));
window.readRequests=[];
const api={contextDebug:async()=>({enabled:true}),requestContexts:async()=>({items,has_more:false}),
requestContext:async(task,id)=>{window.readRequests.push(id);return {metadata:items.find(i=>i.id===id),body:{messages:[{role:'user',content:'Page repair '+id}],tools:[]}}}};
createRoot(document.getElementById('root')).render(React.createElement(CordisProvider,{context:{agentApi:api}},
React.createElement(FlowView,{events,task:{task_id:'test-task',status:'running'},running:true})));
</script></body></html>`)
        res.setHeader('Content-Type', 'text/html'); res.end(html)
      })
    } }],
  })
  let browser
  try {
    await server.listen()
    browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_EXECUTABLE_PATH ? { executablePath: process.env.PLAYWRIGHT_EXECUTABLE_PATH } : {}) })
    const page = await browser.newPage()
    const errors = []
    page.on('pageerror', error => errors.push(String(error)))
    for (const viewport of [{ width: 1440, height: 900 }, { width: 390, height: 844 }]) {
      await page.setViewportSize(viewport)
      await page.goto(server.resolvedUrls.local[0] + 'task-tree-test')
      await page.locator('[data-node-id="task:repair"]').waitFor()
      await page.waitForFunction(() => document.querySelectorAll('.react-flow__node').length === 5)
      assert.equal(await page.locator('[data-node-id="task:n0"] [title="挂起，正在执行子任务"]').count(), 1)
      assert.equal(await page.locator('[data-node-id="task:decision"] [title="等待用户决定"]').count(), 1)
      await page.waitForFunction(() => document.querySelector('.react-flow__viewport')?.style.transform.includes('scale('))
      const boxes = await page.locator('.react-flow__node').evaluateAll(nodes => nodes.map(node => {
        const b = node.getBoundingClientRect(); return { x: b.x, y: b.y, right: b.right, bottom: b.bottom }
      }))
      for (let i = 0; i < boxes.length; i++) for (let j = i + 1; j < boxes.length; j++) {
        const a = boxes[i], b = boxes[j]
        assert.ok(a.right <= b.x || b.right <= a.x || a.bottom <= b.y || b.bottom <= a.y, 'task cards overlap')
      }
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true)
      await page.screenshot({ path: output + `tree-${viewport.width}.png` })
      await page.getByTitle('点击查看当前进行中节点详情').click()
      const requests = page.getByLabel('选择模型请求')
      await page.waitForFunction(() => document.querySelector('select[aria-label="选择模型请求"]')?.options.length === 3)
      assert.deepEqual(await requests.locator('option').evaluateAll(options => options.map(option => option.value)), ['', '3', '1'])
      await requests.selectOption('3')
      await page.getByRole('checkbox').check()
      await page.waitForFunction(() => window.readRequests.includes(1))
      assert.equal(await page.evaluate(() => window.readRequests.includes(2)), false)
      await page.screenshot({ path: output + `requests-${viewport.width}.png` })
    }
    assert.deepEqual(errors, [])
  } finally {
    if (browser) await browser.close()
    await server.close()
  }
})
