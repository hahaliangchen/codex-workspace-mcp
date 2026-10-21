import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import assert from 'node:assert/strict';
import { syncDesign, convertDesign } from '/home/goodtime/project/pptx-editor-engine/src/design/service';
import { projectPage } from '/home/goodtime/project/pptx-editor-engine/src/design/projection';
import { templateSemanticProposal } from '/home/goodtime/project/pptx-editor-engine/src/design/template-semantics';
import { effectivePurpose } from '/home/goodtime/project/pptx-editor-engine/src/design/template-policy';
import { HeadlessSession } from '/home/goodtime/project/pptx-editor-engine/src/headless/session';
const out='/tmp/ppt-template-closed-loop-20261022';mkdirSync(out,{recursive:true});const runStarted=Date.now();
const source=readFileSync('/home/goodtime/project/pptx-editor-engine/tests/fixtures/unicom-uniai-template.pptx');
const repairOnly=process.argv.includes('--repair');const timings:any[]=[];let design=await syncDesign(source);
// This is a new acceptance copy. Keep the source's brand-content page as a valid single-page template.
design=await convertDesign({operation:'pages',design,expectedRevision:design.revision,commands:[{op:'deleteSlide',slide:4},{op:'deleteSlide',slide:2},{op:'deleteSlide',slide:1}]});
let html=projectPage(design,design.pages[0].id),dom=new DOMParser().parseFromString(html,'text/html');
dom.body.dataset.pageRole='content';dom.body.dataset.layoutName='联通品牌空白页';dom.body.dataset.pageNote='保留页眉品牌，其余区域按新内容自由设计，不限制页数。';
for(const e of dom.querySelectorAll<HTMLElement>('[data-id]')) {const n=design.pages[0].nodes.find(n=>n.id===e.dataset.id)!; if(n.metadata?.backdrop || n.hidden)continue; e.dataset.purpose=n.kind==='text'?'content':'fixed';e.dataset.note=n.kind==='text'?'可改文字及布局':'品牌装饰或原始素材，原样保留';}
dom.body.innerHTML=dom.body.innerHTML.replace('{{PAGE_TITLE}}','交付管理复盘').replace('{{PAGE_NUM}}','01');
let edited='<!doctype html>'+dom.documentElement.outerHTML;
const proposal=templateSemanticProposal(design,html,edited);
design=await convertDesign({operation:'apply',design,expectedRevision:design.revision,html:edited,semanticAuthorization:{...proposal,mode:'template-authoring'}});
writeFileSync(join(out,'unicom-brand-template.pptx'),Buffer.from(design.source.pptxBase64,'base64'));
const template=await syncDesign(Buffer.from(design.source.pptxBase64,'base64'));
const body=(text:string,size=24,color='#222B38',weight=400)=>`<p style="margin:0;font-family:'Noto Sans CJK SC';font-size:${size}px;line-height:${Math.ceil(size*1.42)}px;color:${color};font-weight:${weight}">${text.replaceAll("\n","<br>")}</p>`;
const text=(x:number,y:number,w:number,h:number,words:string,size=24,color='#222B38',weight=400)=>`<div data-kind="text" style="position:absolute;left:${x}px;top:${y}px;width:${w}px;height:${h}px">${body(words,size,color,weight)}</div>`;
const rect=(x:number,y:number,w:number,h:number,fill:string)=>`<div data-kind="shape" style="position:absolute;left:${x}px;top:${y}px;width:${w}px;height:${h}px;background:${fill}"></div>`;
const data=[
 ['为什么工作总被推到晚上','用交付机制解释加班现象','观察需求、协作与排期的叠加关系','这是待验证的机制分析，不代表对企业或个人的事实断言。'],
 ['先看交付链条，而非个人忙闲','需求变更','承诺不变，工作量持续增加','影响评估缺位时，新增工作挤占原计划。'],
 ['计划失去缓冲，风险在末端集中','需求确认','开发联调','测试验收'],
 ['等待与返工，消耗连续工作时间','决策等待','接口分歧','碎片沟通'],
 ['把改进放到白天的工作机制中','明确变更入口','建立接口清单','让工作负荷可见'],
 ['用共同事实确定问题范围','观察一周的任务流转','区分新增、等待、返工与专注工作','先取得团队共识，再决定优先改进哪一类损耗。'],
 ['需求变更需要完整的影响评估','范围变化','资源变化','交付时间变化'],
 ['交付承诺应当有清晰的边界','定义完成标准','明确责任人','记录变更依据'],
 ['资源并行不等于产出并行','减少关键角色同时在制任务','保护连续工作时间','观察排队位置，而不是只观察每个人有多忙。'],
 ['联调问题应在接口阶段暴露','共同确认字段和样例','保留可执行的验收用例','边界清晰，能够减少临近交付时的重复解释。'],
 ['等待决策的任务需要升级路径','明确谁来决定','约定响应时限','超时主动升级'],
 ['沟通记录应当落到任务上','集中结论','标明下一步','记录完成标准'],
 ['会议要帮助结束问题','输入：明确需要决定什么','输出：责任人、动作与时间','没有决策需求的状态同步，可以用书面记录替代。'],
 ['把验收准备前移到计划阶段','需求确认时确定验收依据','开发中同步形成测试样例','交付前处理剩余风险，而不是首次讨论是否达标。'],
 ['紧急任务需要显式交换优先级','新增紧急事项','重新确认受影响任务','说明哪些事项延期，以及由谁作出决定。'],
 ['不以在线时长代替协作质量','看任务能否顺利流转','看信息是否完整可追溯','评价实际结果，避免把即时回复变成隐性的工作要求。'],
 ['从一个小范围试点开始','选一个交付小组','只改一项关键机制','用实际工作体验和流转记录检查是否改善。'],
 ['每周复盘关注机制而不是归责','本周最常见的阻塞是什么','哪个约定没有起到作用','下一周只选一个可以验证的改动。'],
 ['用可观察的信号判断改进','变更是否及时评估','等待是否得到响应','风险是否提前暴露'],
 ['减少加班，需要减少工作的损耗','保护合理的计划缓冲','改善跨团队协作','以团队事实验证假设，持续调整交付机制。'],
];
function layout(i:number,count:number){const [title,a,b,c]=data[i];let s='';
 s+=text(66,142,1140,72,title,38,'#1A2433',700);
 s+=text(68,669,1030,30,'机制分析 · 需结合实际项目记录验证',14,'#6B7280');
 if(i===0){s+=rect(66,247,712,332,'#FFF2F1')+text(100,280,645,88,a,36,'#BE123C',700)+text(100,395,635,80,b,26)+text(100,501,625,50,c,17,'#6B7280');s+=text(840,268,350,70,'观察 → 理解 → 改进',28,'#BE123C',700)+text(840,360,350,150,'从需求边界到协作接口，\n从计划缓冲到决策责任。',24)+text(840,550,350,40,`${count} 页讨论提纲`,20,'#6B7280');}
 else if(i%4===1){s+=rect(66,247,380,350,'#BE123C')+text(95,277,320,110,a,32,'#FFFFFF',700)+text(95,478,310,70,'问题应放回交付上下文理解',21,'#FFFFFF');s+=text(500,270,690,110,b,32,'#1A2433',700)+rect(500,415,680,3,'#E8C8CD')+text(500,450,680,135,c,25);}
 else if(i%4===2 && Math.max(a.length,b.length,c.length)<=15){for(let k=0;k<3;k++){const x=66+k*397;s+=rect(x,264,354,222,k===1?'#FFF2F1':'#F3F5F7')+text(x+25,284,300,42,`0${k+1}`,25,'#BE123C',700)+text(x+25,348,300,96,[a,b,c][k],29,'#1A2433',700);if(k<2)s+=text(x+358,350,35,48,'→',27,'#BE123C');}s+=text(68,544,1135,70,'把依赖与完成标准前移，减少风险在交付末端集中。',25);}
 else if(i%4===3 && Math.max(a.length,b.length,c.length)<=14){for(let k=0;k<3;k++){const y=252+k*121;s+=text(70,y,70,60,`0${k+1}`,34,'#BE123C',700)+text(175,y,440,70,[a,b,c][k],30,'#1A2433',700)+rect(68,y+92,545,2,'#E9EAED');}s+=text(710,256,470,310,'先让阻塞可见，\n再明确责任和协作约定。\n\n改善机制，需要观察实际结果。',27,'#556171');}
 else{s+=rect(66,252,1140,326,'#FFF7F6');for(let k=0;k<3;k++){const y=281+k*94;s+=text(104,y,70,50,`0${k+1}`,27,'#BE123C',700)+text(218,y,920,64,[a,b,c][k],28,'#1A2433',k===0?700:400);}}
 return s;
}
for(const count of repairOnly?[]:[5,20]){
 let d=await syncDesign(Buffer.from(template.source.pptxBase64,'base64'));let start=Date.now();
 d=await convertDesign({operation:'pages',design:d,expectedRevision:d.revision,commands:Array.from({length:count-1},()=>({op:'duplicateSlide' as const,slide:1}))});
 timings.push({step:'copy',count,ms:Date.now()-start});
 for(let i=0;i<count;i++){
  const pg=d.pages[i],fixed=pg.nodes.filter(n=>effectivePurpose(pg,n)==='fixed').map(n=>[n.id,JSON.stringify(n.appearance)]);
  const dom=new DOMParser().parseFromString(projectPage(d,pg.id),'text/html');
  for(const node of pg.nodes.filter(n=>n.kind==='text')){
   const el=Array.from(dom.querySelectorAll<HTMLElement>('[data-id]')).find(e=>e.dataset.id===node.id)!;
   // Retain header and page number as content; replace sample wording.
   el.innerHTML=body(node.name==='TextBox 8'?'交付管理 · 机制复盘':`${i+1} / ${count}`,node.name==='TextBox 8'?24:15,node.name==='TextBox 8'?'#BE123C':'#6B7280',node.name==='TextBox 8'?700:400);
   el.style.padding='0';el.style.justifyContent='center';
  }
  dom.body.insertAdjacentHTML('beforeend',layout(i,count));
  console.log(`Applying ${count} page ${i+1}`);writeFileSync(join(out,'current-input.html'),'<!doctype html>'+dom.documentElement.outerHTML);start=Date.now();d=await convertDesign({operation:'apply',design:d,expectedRevision:d.revision,html:'<!doctype html>'+dom.documentElement.outerHTML});
  timings.push({step:'html',count,page:i+1,ms:Date.now()-start});
  for(const [id,appearance]of fixed)assert.equal(JSON.stringify(d.pages[i].nodes.find(n=>n.id===id)!.appearance),appearance);
  writeFileSync(join(out,`page-${count}-${i+1}.html`),projectPage(d,d.pages[i].id));
 }
 const bytes=Buffer.from(d.source.pptxBase64,'base64');writeFileSync(join(out,`unicom-${count}-pages.pptx`),bytes);
 start=Date.now();const reopened=await syncDesign(bytes);timings.push({step:'reopen',count,ms:Date.now()-start});assert.equal(reopened.pages.length,count);assert(!JSON.stringify(reopened.pages).includes('{{'));
 start=Date.now();const render=await HeadlessSession.open(bytes,{quiet:true});for(let i=1;i<=count;i++)writeFileSync(join(out,`page-${count}-${i}.png`),await render.render(i,{width:1280}));render.dispose();timings.push({step:'render',count,ms:Date.now()-start});
 console.log(`Saved and rendered ${count} pages with preserved branding.`);
}
if(!repairOnly)writeFileSync(join(out,'verification.json'),JSON.stringify({source:'china_unicom_uniai',sourceUnchanged:Buffer.compare(source,readFileSync('/home/goodtime/project/pptx-editor-engine/tests/fixtures/unicom-uniai-template.pptx'))===0,counts:[5,20],timedCounts:[5,20],elapsedMs:Date.now()-runStarted,timings,method:'AI-authored HTML through shared converter; independent of live product generation'},null,2));

// Author-level typographic refinement: explicit line breaks avoid lone final glyphs in two narrow headings.
let finalDesign=await syncDesign(readFileSync(join(out,'unicom-20-pages.pptx')));
const refinements:any[]=[];
for(const [pageIndex,oldText,newText] of [[13,'需求确认时确定验收依据','需求确认时\n确定验收依据'],[17,'本周最常见的阻塞是什么','本周最常见的\n阻塞是什么']] as const){
 const page=finalDesign.pages[pageIndex]; const node=page.nodes.find(n=>n.appearance.text?.paragraphs.map(p=>p.runs.map(r=>r.text).join('')).join('').replace(/\s/g,'')===oldText);assert(node);
 const dom=new DOMParser().parseFromString(projectPage(finalDesign,page.id),'text/html');
 const element=Array.from(dom.querySelectorAll<HTMLElement>('[data-id]')).find(e=>e.dataset.id===node.id)!;
 element.innerHTML=body(newText,32,'#FFFFFF',700);const started=Date.now();
 finalDesign=await convertDesign({operation:'apply',design:finalDesign,expectedRevision:finalDesign.revision,html:'<!doctype html>'+dom.documentElement.outerHTML});
 refinements.push({page:pageIndex+1,ms:Date.now()-started,reason:'explicit author line break, no font shrinking'});
 writeFileSync(join(out,`page-20-${pageIndex+1}.html`),projectPage(finalDesign,finalDesign.pages[pageIndex].id));
}
const finalBytes=Buffer.from(finalDesign.source.pptxBase64,'base64');writeFileSync(join(out,'unicom-20-pages.pptx'),finalBytes);
const finalRender=await HeadlessSession.open(finalBytes,{quiet:true});for(const i of [14,18])writeFileSync(join(out,`page-20-${i}.png`),await finalRender.render(i,{width:1280}));finalRender.dispose();
writeFileSync(join(out,'typographic-refinements.json'),JSON.stringify(refinements,null,2));
console.log('Final heading line breaks refined through HTML apply.');
