import assert from 'node:assert/strict'
import test from 'node:test'
import {fileURLToPath} from 'node:url'
import {createServer} from 'vite'
const {chromium}=await import(process.env.PLAYWRIGHT_MODULE||'../../../ai-ppt-view/node_modules/playwright/index.mjs')

test('host composer selects, pastes, drops, removes and submits DSH receipt attachments',async()=>{
 const server=await createServer({root:fileURLToPath(new URL('../',import.meta.url)),configFile:fileURLToPath(new URL('../vite.config.ts',import.meta.url)),server:{port:0,host:'127.0.0.1'},plugins:[{name:'attachment-test',configureServer(server){server.middlewares.use(async(req,res,next)=>{
  if(req.url!=='/agent/attachment-test')return next()
  const html=await server.transformIndexHtml(req.url,`<!doctype html><meta charset="utf-8"><div id="root"></div><script type="module">
import React from 'react';import {createRoot} from 'react-dom/client';
import {Composer} from '/agent/src/components/Composer.tsx';import {CordisProvider} from '/agent/src/cordis/react.tsx';import {api} from '/agent/src/api.ts';
const ctx={agentApi:{commands:async()=>({commands:[]})}};
async function submit(prompt,mode,files){try{const content=[];for(const f of files.files){const r=await api.upload('test-task',f,files.signal,p=>files.onProgress('上传 '+p+'%'));content.push({type:'file',receiptId:r.receiptId});}await api.prompt('test-task',prompt,{content,requestId:files.requestId});return true;}catch(e){window.lastSubmitError=e.message;return false;}}
createRoot(document.getElementById('root')).render(React.createElement(CordisProvider,{context:ctx},React.createElement(Composer,{variant:'hero',disabled:false,running:false,onSubmit:submit,note:undefined})));
</script>`);res.setHeader('Content-Type','text/html');res.end(html)
 })}}]})
 let browser
 try{
  await server.listen();const origin=server.resolvedUrls.local[0];browser=await chromium.launch({headless:true});const page=await browser.newPage();page.setDefaultTimeout(15000);const errors=[];page.on('pageerror',e=>errors.push(String(e)))
  const uploads=[],prompts=[],slow=[];let fail=false
  await page.route('**/api/session/uploadFileBinary?*',async route=>{const req=route.request();assert.equal(req.headers()['content-type'],'application/octet-stream');const url=new URL(req.url());assert.equal(url.searchParams.get('sessionId'),'test-task');uploads.push({name:url.searchParams.get('name'),bytes:req.postDataBuffer()});if(url.searchParams.get('name')==='cancel.txt'){slow.push(route);return;}await route.fulfill({json:{ok:true,value:{receiptId:'receipt-'+uploads.length,file:{attachmentId:'sha256:test',name:uploads.at(-1).name,bytes:uploads.at(-1).bytes.length}}}})})
  await page.route('**/agent/tasks/test-task/messages',async route=>{prompts.push(route.request().postDataJSON());await route.fulfill({status:fail?500:202,json:fail?{error:'test rejection'}:{task_id:'test-task'}})})
  await page.goto(origin+'attachment-test');const input=page.locator('textarea');const send=page.getByRole('button',{name:'发送消息',exact:true})
  await page.getByLabel('上传附件').setInputFiles({name:'brief.md',mimeType:'text/markdown',buffer:Buffer.from('# 素材')});await page.getByRole('button',{name:'移除 brief.md'}).click();assert.equal(await send.count(),0)
  const image=await input.evaluate(el=>{const c=document.createElement('canvas');c.width=12;c.height=8;c.getContext('2d').fillRect(0,0,12,8);const data=c.toDataURL().split(',')[1];const d=new DataTransfer();d.items.add(new File([Uint8Array.from(atob(data),x=>x.charCodeAt(0))],'paste.png',{type:'image/png'}));el.dispatchEvent(new ClipboardEvent('paste',{clipboardData:d,bubbles:true,cancelable:true}));return data})
  await page.getByRole('img',{name:'paste.png'}).waitFor();await send.click();await page.getByRole('button',{name:'移除 paste.png'}).waitFor({state:'hidden'});assert.deepEqual(uploads[0].bytes,Buffer.from(image,'base64'));assert.equal(prompts[0].prompt,'');assert.equal(prompts[0].content[0].receiptId,'receipt-1')
  await input.evaluate(el=>{const d=new DataTransfer();d.items.add(new File(['需求说明'],'drop.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:d,bubbles:true,cancelable:true}))});await page.getByRole('button',{name:'移除 drop.txt'}).waitFor();await input.fill('根据文档');fail=true;await send.click();await page.waitForFunction(()=>window.lastSubmitError==='test rejection');await page.getByRole('button',{name:'移除 drop.txt'}).waitFor();assert.equal(await input.inputValue(),'根据文档');fail=false;await page.locator('button[aria-label="发送消息"]:not(:disabled)').click();await page.getByRole('button',{name:'移除 drop.txt'}).waitFor({state:'hidden'});assert.equal(prompts[1].requestId,prompts[2].requestId);
  await page.getByLabel('上传附件').setInputFiles({name:'cancel.txt',mimeType:'text/plain',buffer:Buffer.from('取消上传不提交模型')});await send.click();await page.getByRole('button',{name:'取消上传',exact:true}).click();await page.waitForFunction(()=>window.lastSubmitError==='上传已取消');await page.getByRole('button',{name:'移除 cancel.txt'}).waitFor();assert.equal(prompts.length,3,'Cancelled uploads never submit a prompt');for(const route of slow)await route.abort().catch(()=>{});assert.deepEqual(errors,[])
 }finally{if(browser)await browser.close();await server.close()}
})
