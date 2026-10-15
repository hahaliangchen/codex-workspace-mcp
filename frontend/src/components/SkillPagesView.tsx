import { useState } from 'react'
import { useAgentApi } from '../cordis/react.tsx'
import { usePluginPages } from '../plugins/react.tsx'

export function SkillPagesView({ workspaceRoot }: { workspaceRoot: string }) {
  const api = useAgentApi()
  const { packages, pages, errors } = usePluginPages()
  const [selected, setSelected] = useState<{ owner: string; page: string }>()
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const [installPath, setInstallPath] = useState('')
  const [remoteUrl, setRemoteUrl] = useState('')
  const [remoteToken, setRemoteToken] = useState('')
  const owner = packages.find(item => item.id === selected?.owner && item.enabled)
  const page = owner?.pages.find(item => item.id === selected?.page)
  const contribution = pages.find(item => item.owner === owner?.id && item.scope === owner?.scope && item.id === page?.id)
  const Page = contribution?.component
  async function change(action: () => Promise<unknown>) {
    setBusy(true); setError('')
    try { await action() }
    catch (reason) { setError(String(reason)) }
    finally { setBusy(false) }
  }
  return <section aria-label="技能插件页面" style={{ width: '100%', height: '100%', minHeight: 520, display: 'flex', flexDirection: 'column' }}>
    <div style={{ padding: '10px 14px', display: 'flex', gap: 10, flexWrap: 'wrap', borderBottom: '1px solid #e5e7eb' }}>
      {packages.map(item => <div key={item.id} style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
        <span title={item.description}>{item.title}</span><small>{item.origin === 'builtin' ? '内置' : '外置'}</small>
        <button disabled={busy} onClick={() => { void change(() => api.setPackageEnabled(item.id, !item.enabled)) }}>{item.enabled ? '停用插件' : '启用插件'}</button>
        <button disabled={busy} onClick={() => { void change(() => api.reloadPackage(item.id)) }}>重载</button>
        {item.origin === 'external' && <button disabled={busy} onClick={() => { void change(() => api.removePackage(item.id)) }}>移除</button>}
        {item.enabled && item.pages.map(page => <button key={page.id} onClick={() => setSelected({ owner: item.id, page: page.id })}>{page.title}</button>)}
      </div>)}
    </div>
    <form onSubmit={event => { event.preventDefault(); void change(() => api.installPackage(installPath.trim())) }} style={{ padding: '8px 14px', display: 'flex', gap: 8 }}>
      <input aria-label="本地技能包目录" placeholder="本地技能包目录（含 plugin.json）" value={installPath} onChange={event => setInstallPath(event.target.value)} style={{ flex: 1 }} />
      <button disabled={busy || !installPath.trim()}>安装技能包</button>
    </form>
    <form onSubmit={event => { event.preventDefault(); void change(async () => { await api.connectRemotePackage(remoteUrl.trim(), remoteToken); setRemoteToken('') }) }} style={{ padding: '8px 14px', display: 'flex', gap: 8 }}>
      <input aria-label="远程插件服务地址" placeholder="远程 PPT 服务地址（…/aiPpt/plugin）" value={remoteUrl} onChange={event => setRemoteUrl(event.target.value)} style={{ flex: 1 }} />
      <input type="password" aria-label="远程服务凭证" placeholder="服务凭证" value={remoteToken} onChange={event => setRemoteToken(event.target.value)} />
      <button disabled={busy || !remoteUrl.trim()}>握手并连接</button>
    </form>
    {(error || Object.values(errors).join('；')) && <p role="alert" style={{ color: '#b91c1c', padding: 12 }}>{error || Object.values(errors).join('；')}</p>}
    <div style={{ flex: 1, minHeight: 0, display: 'flex' }}>
      {!page ? <p style={{ padding: 20 }}>选择一个技能包的页面。页面和工具随所属插件停用；打开页面不会给 Agent 自动加载技能。</p>
        : Page && owner ? <Page key={`${owner.id}:${owner.scope}:${page.id}`} owner={owner} pageId={page.id} workspaceRoot={workspaceRoot} /> : <p style={{ padding: 20 }}>等待插件注册页面贡献…</p>}
    </div>
  </section>
}
