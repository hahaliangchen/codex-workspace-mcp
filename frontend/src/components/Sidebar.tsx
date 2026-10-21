import { useEffect, useMemo, useRef, useState } from 'react'
import { api } from '../api.ts'
import clsx from 'clsx'
import { Button, Menu, Modal, IconFolderCloseRegular, IconFolderOpenRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import type { Project, Task, TaskStatus } from '../api.ts'
import { isChat, projectGroups, taskTitle, visibleTasks } from '../sidebarModel.ts'
export { taskTitle, workspaceName } from '../sidebarModel.ts'
import css from './CodexSidebar.module.css'
import { ProjectNotesDialog } from './ProjectNotesDialog.tsx'
import { MemoryDialog } from './MemoryDialog.tsx'
import sproutLogo from '../assets/sprout.svg'

export function dotState(status: TaskStatus): 'done' | 'warning' | 'ongoing' | 'error' | 'idle' {
  switch (status) {
    case 'running': case 'cancelling': return 'ongoing'
    case 'completed': return 'done'
    case 'failed': case 'interrupted': return 'error'
    case 'max_steps': case 'cancelled': return 'warning'
    case 'draft': return 'idle'
  }
}

type Dialog = {kind: 'rename-task' | 'delete-task'; task: Task} | {kind: 'rename-project' | 'delete-project'; project: Project}
function RowMenu({ label, items, onSelect }: {label: string; items: {id: string; label: string; danger?: boolean}[]; onSelect: (id: string) => void}) {
  const [open, setOpen] = useState(false)
  return <Menu open={open} onClose={() => setOpen(false)} items={items} onSelect={id => { setOpen(false); onSelect(id) }} align="end" portal dense
    anchor={<button type="button" className={clsx(css.sectionActionBtn, css.rowMenu)} aria-label={label} title={label} onClick={() => setOpen(value => !value)}>···</button>} />
}

export function Sidebar({
  tasks, allTasks: _allTasks, selectedId, activeTaskId, workspace, projects = [], activeWorkspace,
  onSelectWorkspace, onAddWorkspace, onRemoveWorkspace, onRenameWorkspace,
  mode = 'code', onModeChange, onUpdateSession, onDeleteSession,
  collapsed = false, width = 260, onToggleCollapse, onNew, onSelect, onOpenSettings,
}: {
  tasks: readonly Task[]
  allTasks?: readonly Task[] | undefined
  selectedId: string | undefined
  activeTaskId?: string | undefined
  workspace: string
  workspaces?: readonly string[] | undefined
  projects?: readonly Project[] | undefined
  activeWorkspace?: string | undefined
  onSelectWorkspace?: ((ws: string) => void) | undefined
  onAddWorkspace?: (() => void) | undefined
  onRemoveWorkspace?: ((path: string) => Promise<void>) | undefined
  onRenameWorkspace?: ((path: string, title: string) => Promise<void>) | undefined
  mode?: 'chat' | 'code' | undefined
  onModeChange?: ((mode: 'chat' | 'code') => void) | undefined
  onUpdateSession?: ((id: string, input: {title?: string; pinned?: boolean; archived?: boolean}) => Promise<void>) | undefined
  onDeleteSession?: ((id: string) => Promise<void>) | undefined
  dark?: boolean | undefined
  collapsed?: boolean | undefined
  width?: number | undefined
  onToggleCollapse?: (() => void) | undefined
  onNew: () => void
  onSelect: (taskId: string) => void
  onToggleTheme?: (() => void) | undefined
  onOpenSettings?: (() => void) | undefined
}) {
  const [searchOpen, setSearchOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [closed, setClosed] = useState<Record<string, boolean>>({})
  const [archived, setArchived] = useState(false)
  const [dialog, setDialog] = useState<Dialog>()
  const [notesProject, setNotesProject] = useState<Project>()
  const [memoryProject, setMemoryProject] = useState<Project>()
  const [draft, setDraft] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState('')
  const searchRef = useRef<HTMLInputElement>(null)
  const currentId = activeTaskId ?? selectedId
  // Titles filter instantly; message content is searched on the server after a pause.
  const [snippets, setSnippets] = useState<ReadonlyMap<string, string>>(new Map())
  useEffect(() => {
    const q = query.trim()
    if ([...q].length < 2) { setSnippets(new Map()); return }
    let live = true
    const timer = window.setTimeout(() => {
      api.searchSessions(q).then(result => { if (live) setSnippets(new Map(result.matches.map(match => [match.task_id, match.snippet]))) }, () => {})
    }, 250)
    return () => { live = false; window.clearTimeout(timer) }
  }, [query])
  const contentHits = useMemo(() => new Set(snippets.keys()), [snippets])
  const filtered = visibleTasks(tasks, query, archived, contentHits)
  const {groups, ungrouped} = projectGroups(projects, filtered, workspace)
  const chats = filtered.filter(isChat)
  const request = (next: Dialog) => {
    setError('')
    setDraft('task' in next ? taskTitle(next.task) : next.project.title)
    setDialog(next)
  }
  const run = async (action: () => Promise<void> | undefined) => {
    setError('')
    setBusy(true)
    try { await action(); setDialog(undefined) } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)) }
    finally { setBusy(false) }
  }
  const confirm = () => {
    if (!dialog) return
    void run(() => {
      switch (dialog.kind) {
        case 'rename-task': return onUpdateSession?.(dialog.task.task_id, {title: draft.trim()})
        case 'delete-task': return onDeleteSession?.(dialog.task.task_id)
        case 'rename-project': return onRenameWorkspace?.(dialog.project.path, draft.trim())
        case 'delete-project': return onRemoveWorkspace?.(dialog.project.path)
      }
    })
  }
  const sessionRow = (task: Task, nested = false) => {
    const running = task.status === 'running' || task.status === 'cancelling'
    return <div key={task.task_id} className={clsx(css.itemRow, task.task_id === currentId && css.sessionActive)}>
      <button type="button" className={nested ? css.sessionRow : css.recentRow} onClick={() => onSelect(task.task_id)} title={taskTitle(task)}>
        <span className={css.sessionTitle}>{taskTitle(task)}{query.trim() && snippets.has(task.task_id) && <span className={css.searchSnippet}>{snippets.get(task.task_id)}</span>}</span>
        {task.pinned && <span className={css.pinMark} aria-label="已置顶">⌃</span>}
        {task.awaiting_approval ? <span className={css.awaitingMark} title="等你确认是否运行命令">待确认</span> : running && <span className={css.runningSpinner} aria-label="执行中">◌</span>}
      </button>
      <RowMenu label={`会话操作：${taskTitle(task)}`} items={[
        {id:'rename', label:'重命名'}, {id:'pin', label: task.pinned ? '取消置顶' : '置顶'},
        {id:'archive', label: task.archived ? '取消归档' : '归档'}, {id:'delete', label:'删除会话', danger:true},
      ]} onSelect={id => {
        if (id === 'rename') request({kind:'rename-task',task})
        else if (id === 'delete') request({kind:'delete-task',task})
        else void run(() => onUpdateSession?.(task.task_id, id === 'pin' ? {pinned:!task.pinned} : {archived:!task.archived}))
      }} />
    </div>
  }
  const renaming = dialog?.kind.startsWith('rename')
  if (collapsed) return <div className={clsx(css.root, css.collapsedRoot)}><button type="button" className={css.iconBtn} title="展开侧边栏" onClick={onToggleCollapse}>☰</button></div>
  return <aside className={css.root} style={{width}} aria-label="会话侧栏">
    <div className={css.header}>
      <span className={css.brandTitle}><img src={sproutLogo} width={18} height={18} alt="" />Composer</span>
      <div className={css.headerActions}>
        <button type="button" className={css.iconBtn} title="搜索会话" aria-label="搜索会话" onClick={() => {setSearchOpen(value => !value); window.setTimeout(() => searchRef.current?.focus(),0)}}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><circle cx="10.5" cy="10.5" r="7.5"/><path d="m16 16 5 5"/></svg>
        </button>
        <button type="button" className={css.iconBtn} title="设置" aria-label="设置" onClick={onOpenSettings}>⚙</button>
        <button type="button" className={css.iconBtn} title="折叠侧边栏" aria-label="折叠侧边栏" onClick={onToggleCollapse}>
          <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><rect x="3" y="3" width="18" height="18" rx="2"/><path d="M9 3v18"/></svg>
        </button>
      </div>
    </div>
    <div className={css.newChatRow}>
      <button type="button" className={css.newChatBtn} onClick={onNew}><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8"><path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7M16 3l5 5-9 9-5 1 1-5Z"/></svg>{mode === 'chat' ? '新聊天' : '新代码会话'}</button>
      <div className={css.modeSwitch} role="tablist" aria-label="会话类型">
        <button type="button" role="tab" aria-selected={mode === 'chat'} aria-label="聊天" title="聊天" className={clsx(css.modeButton, mode === 'chat' && css.modeActive)} onClick={() => onModeChange?.('chat')}>
          <svg width="18" height="18" viewBox="0 0 24 24" aria-hidden="true"><path d="M12 3.5c-5 0-9 3.4-9 7.6 0 2.2 1.1 4.2 2.9 5.6L5 20.5l4.3-2.1c.9.2 1.8.3 2.7.3 5 0 9-3.4 9-7.6s-4-7.6-9-7.6Z" fill="#5DCAA5"/><circle cx="8.2" cy="11.1" r="1.15" fill="#fff"/><circle cx="12" cy="11.1" r="1.15" fill="#fff"/><circle cx="15.8" cy="11.1" r="1.15" fill="#fff"/></svg>
        </button>
        <button type="button" role="tab" aria-selected={mode === 'code'} aria-label="代码" title="代码" className={clsx(css.modeButton, mode === 'code' && css.modeActive)} onClick={() => onModeChange?.('code')}>
          <svg width="18" height="18" viewBox="0 0 64 64" aria-hidden="true"><path d="M20 18 8 32l12 14M44 18l12 14-12 14" stroke="currentColor" strokeWidth="5" strokeLinecap="round" strokeLinejoin="round" fill="none"/><path d="M24 44C22 30 30 20 42 18c2 14-6 26-18 26Z" fill="#1D9E75"/><path d="M24 44 36 28" stroke="#E1F5EE" strokeWidth="2.5" strokeLinecap="round"/></svg>
        </button>
      </div>
    </div>
    {searchOpen && <div className={css.searchBox}><input ref={searchRef} className={css.searchInput} placeholder="搜索标题和聊天内容…" aria-label="搜索会话标题和内容" value={query} onChange={event => setQuery(event.target.value)} onKeyDown={event => {if(event.key === 'Escape'){setQuery('');setSearchOpen(false)}}}/></div>}
    <div className={css.scrollArea} role="tabpanel" aria-label={mode === 'code' ? '代码会话' : '聊天会话'}>
      <div className={css.sectionHeaderRow}>
        <span className={css.sectionTitleLeft}>{mode === 'code' ? '项目' : '聊天'}{archived ? ' · 已归档' : ''}</span>
        <div className={css.sectionActionsRight}>
          <button type="button" className={css.sectionActionBtn} title={archived ? '返回未归档会话' : '查看已归档会话'} aria-label={archived ? '返回未归档会话' : '查看已归档会话'} onClick={() => setArchived(value => !value)}>▣</button>
          {mode === 'code' && <button type="button" className={css.sectionActionBtn} title="添加项目" aria-label="添加项目" onClick={onAddWorkspace}>＋</button>}
        </div>
      </div>
      {mode === 'code' ? <>
        {groups.filter(group => !query.trim() || group.tasks.length > 0 || group.title.toLocaleLowerCase().includes(query.toLocaleLowerCase().trim())).map(project => <div key={project.path} className={css.projectGroup}>
          <div className={clsx(css.itemRow, css.projectHeaderRow, project.path === activeWorkspace && css.projectHeaderRowActive)}>
            <button type="button" className={css.folderToggle} aria-label={`${closed[project.path] ? '展开' : '折叠'}项目：${project.title}`} aria-expanded={!closed[project.path]} onClick={() => setClosed(current => ({...current,[project.path]:!current[project.path]}))}>{closed[project.path] ? <IconFolderCloseRegular size={15}/> : <IconFolderOpenRegular size={15}/>}</button>
            <button type="button" className={css.projectSelect} title={project.path} onClick={() => onSelectWorkspace?.(project.path)}><span className={css.projectTitle}>{project.title}</span></button>
            <RowMenu label={`项目操作：${project.title}`} items={[{id:'notes', label:'项目备注'},{id:'memory', label:'AI 的记忆'},{id:'rename', label:'重命名'},{id:'delete',label:'移除项目',danger:true}]} onSelect={id => id === 'notes' ? setNotesProject(project) : id === 'memory' ? setMemoryProject(project) : request({kind:id === 'rename' ? 'rename-project' : 'delete-project',project})}/>
          </div>
          {(!closed[project.path] || query.trim()) && <div className={css.projectSubList}>{project.tasks.map(task => sessionRow(task,true))}</div>}
        </div>)}
        {ungrouped.length > 0 && <><div className={css.sectionHeader}>未分组</div>{ungrouped.map(task => sessionRow(task))}</>}
        {groups.length === 0 && ungrouped.length === 0 && <p className={css.emptyState}>{query.trim() ? '没有匹配的会话' : archived ? '暂无归档会话' : '添加项目，开始代码会话'}</p>}
      </> : chats.length ? chats.map(task => sessionRow(task)) : <p className={css.emptyState}>{query.trim() ? '没有匹配的聊天' : archived ? '暂无归档聊天' : '暂无聊天'}</p>}
    </div>
    {error && !dialog && <div className={css.actionError} role="alert">{error}</div>}
    <ProjectNotesDialog project={notesProject} onClose={() => setNotesProject(undefined)} />
    <MemoryDialog project={memoryProject} onClose={() => setMemoryProject(undefined)} />
    <Modal open={!!dialog} onClose={() => {if(!busy)setDialog(undefined)}} title={renaming ? '重命名' : dialog?.kind === 'delete-project' ? '移除项目' : '删除会话'} closeLabel="关闭"
      description={renaming ? '输入新的显示名称' : dialog?.kind === 'delete-project' ? '仅从项目列表移除，目录文件和已有会话会保留。' : '删除此会话及其子任务的历史记录。此操作无法撤销。'}
      footer={<><Button variant="outline" disabled={busy} onClick={() => setDialog(undefined)}>取消</Button><Button variant="primary" disabled={busy || (!!renaming && !draft.trim())} onClick={confirm}>{busy ? '处理中…' : renaming ? '保存' : '确认删除'}</Button></>}>
      {renaming ? <input autoFocus className={css.searchInput} aria-label="新名称" value={draft} disabled={busy} onChange={event => setDraft(event.target.value)} onKeyDown={event => {if(event.key === 'Enter' && draft.trim() && !busy)confirm()}}/> : <p className={css.dialogName}>{draft}</p>}
      {error && <div className={css.actionError} role="alert">{error}</div>}
    </Modal>
  </aside>
}
