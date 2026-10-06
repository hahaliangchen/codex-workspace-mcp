import { useEffect, useState } from 'react'
import type { AgentEvent, NotebookMaterial, NotebookSource, FindingHistoryPage } from '../api.ts'
import { useAgentApi } from '../cordis/react.tsx'
import css from './RequestContextPanel.module.css'

function records(value: unknown): Record<string, unknown>[] {
  const items=Array.isArray(value)?value:typeof value==='object'&&value!==null?Object.values(value):[]
  return items.filter((item): item is Record<string,unknown>=>typeof item==='object'&&item!==null)
}
function time(value: unknown): string {return typeof value==='number'&&value>0?new Date(value).toLocaleString():'时间未记录'}
function FindingList({items}:{items:readonly Record<string,unknown>[]}) {
  return <>{items.length?items.map((item,index)=><article key={`${String(item.id)}-${String(item.revision)}-${index}`}>
    <p><strong>{String(item.topic??item.id??'结论')}</strong> · 修订 {String(item.revision??'—')} · {time(item.confirmed_at??item.recorded_at)}</p>
    <p>{String(item.text??'')}</p>
    <p className={css.hint}>{item.verification_state==='verified'?'已记录确认来源':item.verification_state==='legacy_unreviewed'?'旧记录，确认来源待核对':item.status==='confirmed'?'Worker 已确认的结论':item.verification_state==='source_changed'?'相关源码已变化，待核对':'Worker 的假设或待核对记录'}
      {item.replacement_id?` · 已由 ${String(item.replacement_id)} 替代`:''}</p>
    {Array.isArray(item.conflicts_with)&&item.conflicts_with.length>0&&<p>冲突记录：{item.conflicts_with.join('、')}</p>}
    {Array.isArray(item.conflict_candidates)&&<FindingList items={records(item.conflict_candidates)}/>}
    <details><summary>来源、版本与关联</summary><pre>{JSON.stringify(item,null,2)}</pre></details>
  </article>):<p className={css.hint}>暂无记录</p>}</>
}

export function TaskNotebookPanel({ taskId, events }: { taskId: string; events: readonly AgentEvent[] }) {
  const api=useAgentApi()
  const [open,setOpen]=useState(false)
  const [materials,setMaterials]=useState<readonly NotebookMaterial[]>([])
  const [findings,setFindings]=useState<Record<string, unknown>>({})
  const [source,setSource]=useState<NotebookSource | null>(null)
  const [loading,setLoading]=useState(false)
  const [error,setError]=useState('')
  const [timeline,setTimeline]=useState<FindingHistoryPage | null>(null)
  const revision=[...events].reverse().find(event=>event.type==='worker/work_state')?.seq ?? -1
  useEffect(()=>{
    if (!open) return
    let alive=true
    const timer=setTimeout(()=>{void api.notebook(taskId).then(result=>{
      if(!alive)return
      setMaterials(result.materials);setFindings(result.work);setTimeline(result.work.finding_timeline as FindingHistoryPage);setError('')
    }).catch(reason=>{if(alive)setError(String(reason))})},300)
    return()=>{alive=false;clearTimeout(timer)}
  },[api,taskId,open,revision])
  async function read(id:number,start?:number,column?:number) {
    setLoading(true);setError('')
    try {setSource(await api.notebookMaterial(taskId,id,start,column))} catch(reason){setError(String(reason))}finally{setLoading(false)}
  }
  async function olderHistory() {
    if(!timeline?.has_more||timeline.next_before===null)return
    setLoading(true);setError('')
    try {const next=await api.findingHistory(taskId,timeline.next_before);setTimeline(previous=>({...next,items:[...(previous?.items??[]),...next.items]}))}
    catch(reason){setError(String(reason))}finally{setLoading(false)}
  }
  const all=records(findings.findings)
  const historical=all.filter(item=>item.record_state==='historical'||item.status==='obsolete')
  const conflicts=all.filter(item=>item.record_state==='conflicted'||item.verification_state==='legacy_unreviewed'||item.record_state===undefined)
  const current=all.filter(item=>!historical.includes(item)&&!conflicts.includes(item))
  return <details className={css.panel} onToggle={event=>setOpen(event.currentTarget.open)}>
    <summary>任务记事本 · 结论与源码材料</summary>
    {open&&<>
      <p className={css.hint}>读取结果自动保存。表中是保存时的位置与版本；点击时检查当前文件，未变化的片段可复用。记事本内容不依赖调试开关。</p>
      {error&&<p role="alert" className={css.error}>{error}</p>}
      <details><summary>当前结论 · {current.length}</summary><FindingList items={current}/></details>
      <details><summary>待核对与冲突 · {conflicts.length}</summary><FindingList items={conflicts}/></details>
      <details><summary>被替代的历史结论 · {historical.length}</summary><FindingList items={historical}/></details>
      <details><summary>结论时间线</summary>
        {timeline?.items.map(item=><div key={item.seq}><p>{time(item.record.at??item.time)} · 轮 {String(item.record.turn??'—')} / 步 {String(item.record.step??'—')} · {String(item.record.event??'')}</p><FindingList items={records([item.record.finding])}/></div>)}
        {!timeline?.items.length&&<p className={css.hint}>旧记录未记录修订时间；后续发现、冲突及替代会自动记录。</p>}
        {timeline?.has_more&&<button type="button" disabled={loading} onClick={()=>{void olderHistory()}}>加载更早记录</button>}
      </details>
      <details><summary>历史查询索引</summary><pre>{JSON.stringify(findings.query_results ?? [],null,2)}</pre></details>
      <div style={{overflowX:'auto',maxHeight:300}}><table>
        <thead><tr><th>材料</th><th>位置 / 符号</th><th>节点</th></tr></thead>
        <tbody>{materials.map(material=><tr key={material.id}>
          <td><button type="button" disabled={loading} onClick={()=>{void read(material.id)}}>#{material.id}</button></td>
          <td title={material.file_hash}>{material.path}:{material.start_line}–{material.end_line}<br/>{material.symbol}</td>
          <td>轮 {material.turn} · {material.node_id || '未分配'}</td>
        </tr>)}</tbody>
      </table></div>
      {!materials.length&&<p className={css.hint}>尚未保存材料。新版本执行的源码读取和文件写入会自动记录。</p>}
      {loading&&<p>正在取回并检查版本…</p>}
      {source&&<div><p>#{source.id} · {source.status} · {source.descriptor.path}</p>
        {source.available?<><p className={css.hint}>当前范围 {source.start_line}–{source.end_line}{source.partial_line?` · 长行片段，从第 ${source.start_column ?? 1} 列开始`:''}{source.complete?'':'（尚有未返回内容）'} · 保留原始换行</p><pre>{source.content}</pre>
          {source.next_start_line!=null&&<button type="button" disabled={loading} onClick={()=>{void read(source.id,source.next_start_line ?? undefined,source.next_column ?? undefined)}}>读取下一段</button>}</>
          :<p>该材料已变化、无法唯一定位或文件不存在，不能作为当前源码使用；需要定向刷新。</p>}
      </div>}
    </>}
  </details>
}
