import assert from 'node:assert/strict'
import test from 'node:test'
import { mkdir } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { createServer } from 'vite'

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
