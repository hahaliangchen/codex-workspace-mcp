import { randomId } from '../randomId.ts'
import {useCallback,useEffect,useRef,useState} from 'react'
export interface AttachmentRef {attachmentId:string;name:string;bytes:number;mediaType?:string;width?:number;height?:number}
export interface AttachmentSubmission {files:readonly File[];signal:AbortSignal;requestId:string;onProgress:(label:string)=>void}
export function useAttachmentDraft(extensions?:readonly string[]) {
  const [items,setItems]=useState<{id:string;file:File;preview?:string}[]>([])
  const [error,setError]=useState('')
  const urls=useRef(new Map<string,string>())
  useEffect(()=>()=>{for(const url of urls.current.values())URL.revokeObjectURL(url)},[])
  const add=useCallback((files:Iterable<File>)=>{
    const accepted:{id:string;file:File;preview?:string}[]=[];const errors:string[]=[]
    for(const file of files){
      const ext=file.name.split('.').at(-1)?.toLowerCase()??''
      if(extensions&&!extensions.includes(ext)){errors.push(`不支持 ${file.name}`);continue}
      if(file.size===0||file.size>20*1024*1024){errors.push(`${file.name} 必须非空且不超过 20 MiB`);continue}
      const id=randomId();const preview=file.type.startsWith('image/')?URL.createObjectURL(file):undefined
      if(preview)urls.current.set(id,preview)
      accepted.push({id,file,...(preview?{preview}:{})})
    }
    setItems(old=>{
      const all=[...old,...accepted];for(const item of all.slice(20)){const url=urls.current.get(item.id);if(url)URL.revokeObjectURL(url);urls.current.delete(item.id)}
      return all.slice(0,20)
    });setError(errors.join('；'))
  },[extensions])
  const remove=(id:string)=>{const url=urls.current.get(id);if(url)URL.revokeObjectURL(url);urls.current.delete(id);setItems(old=>old.filter(x=>x.id!==id))}
  const clear=()=>{for(const url of urls.current.values())URL.revokeObjectURL(url);urls.current.clear();setItems([]);setError('')}
  return {items,add,remove,clear,error}
}
export function AttachmentRail({items,onRemove,disabled=false}:{items:readonly {id:string;file:File;preview?:string}[];onRemove:(id:string)=>void;disabled?:boolean}){
 return <div style={{display:'flex',flexWrap:'wrap',gap:8,padding:items.length?'8px 12px':0}}>{items.map(item=><div key={item.id} style={{display:'flex',alignItems:'center',gap:8,maxWidth:240,padding:8,border:'1px solid #dce1e7',borderRadius:10,background:'#f8fafc',color:'#273444'}}>
 {item.preview?<img src={item.preview} alt={item.file.name} style={{width:56,height:44,objectFit:'cover',borderRadius:6}}/>:<span aria-hidden>📎</span>}
 <span style={{fontSize:12,overflow:'hidden',textOverflow:'ellipsis',whiteSpace:'nowrap'}}>{item.file.name}</span><button type='button' aria-label={`移除 ${item.file.name}`} onClick={()=>onRemove(item.id)} disabled={disabled} style={{border:0,background:'transparent',cursor:'pointer'}}>×</button>
 </div>)}</div>
}
