import type {AttachmentSubmission} from './components/Attachments'
import { requestNotificationPermission, useTaskNotifications } from './useTaskNotifications.ts'
import type { PermissionMode } from './api'
import { useCallback, useEffect, useRef, useState } from 'react'
import { acceptsPrompt, type AgentInfo, type SettingsData, type Task, type Project } from './api.ts'
import { useAgentApi } from './cordis/react.tsx'
import { t } from './i18n.ts'
import { useSession } from './useSession.ts'

export type SettingsTab = 'general' | 'models' | 'observer' | 'tools' | 'plugins'

function routeTaskId(): string | undefined {
  const match = /^#\/tasks\/(.+)$/.exec(location.hash)
  return match?.[1] === undefined ? undefined : decodeURIComponent(match[1])
}

function isSettingsRoute(): boolean {
  return location.pathname.endsWith('/settings') || location.hash.startsWith('#/settings')
}

function openTask(taskId: string | undefined): void {
  if (location.pathname.endsWith('/settings')) {
    history.replaceState(null, '', '/agent' + (taskId === undefined ? '#/' : `#/tasks/${encodeURIComponent(taskId)}`))
    window.dispatchEvent(new HashChangeEvent('hashchange'))
    return
  }
  location.hash = taskId === undefined ? '#/' : `#/tasks/${encodeURIComponent(taskId)}`
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause)
}

/** Task selection, history, settings state, and commands shared by the hero and conversation views. */
export function useAgentWorkspace() {
  const api = useAgentApi()
  const [info, setInfo] = useState<AgentInfo>()
  const [settings, setSettings] = useState<SettingsData>()
  const [allTasks, setAllTasks] = useState<readonly Task[]>([])
  const [taskId, setTaskId] = useState(routeTaskId)
  const [notice, setNotice] = useState<string>()
  const [parent, setParent] = useState<Task>()
  const [settingsOpen, setSettingsOpen] = useState(isSettingsRoute)
  const [settingsTab, setSettingsTab] = useState<SettingsTab>(isSettingsRoute() ? 'models' : 'general')
  const [selectedModelOverride, setSelectedModelOverride] = useState<string>()
  const [selectedEffortOverride, setSelectedEffortOverride] = useState<string | null>()
  const [selectedFastModeOverride, setSelectedFastModeOverride] = useState<boolean>()

  const [projects, setProjects] = useState<readonly Project[]>([])
  const workspaces = projects.map(project => project.path)
  const [activeWorkspace, setActiveWorkspace] = useState<string>(() => {
    try { return localStorage.getItem('codex_active_workspace') ?? '' } catch { return '' }
  })
  const [sidebarMode, setSidebarMode] = useState<'chat' | 'code'>(() => {
    try { return localStorage.getItem('codex_sidebar_mode') === 'chat' ? 'chat' : 'code' } catch { return 'code' }
  })
  const changeSidebarMode = useCallback((mode: 'chat' | 'code') => {
    setSidebarMode(mode)
    try { localStorage.setItem('codex_sidebar_mode', mode) } catch {}
  }, [])
  const [folderDialogOpen, setFolderDialogOpen] = useState(false)
  const refreshProjects = useCallback(async () => {
    const list = await api.projects()
    setProjects(list)
    setActiveWorkspace(current => list.some(project => project.path === current) ? current : list[0]?.path ?? '')
  }, [api])
  const selectWorkspace = useCallback((ws: string) => {
    setActiveWorkspace(ws)
    changeSidebarMode('code')
    try { localStorage.setItem('codex_active_workspace', ws) } catch {}
    openTask(undefined)
  }, [changeSidebarMode])
  const addWorkspace = useCallback(async (path: string) => {
    const project = await api.addProject(path.trim())
    await refreshProjects()
    selectWorkspace(project.path)
    setFolderDialogOpen(false)
  }, [api, refreshProjects, selectWorkspace])
  const removeWorkspace = useCallback(async (path: string) => {
    await api.removeProject(path)
    await refreshProjects()
    if (path === activeWorkspace) openTask(undefined)
  }, [api, activeWorkspace, refreshProjects])
  const renameWorkspace = useCallback(async (path: string, title: string) => {
    await api.renameProject(path, title)
    await refreshProjects()
  }, [api, refreshProjects])

  const openFolderDialog = useCallback(() => setFolderDialogOpen(true), [])
  const closeFolderDialog = useCallback(() => setFolderDialogOpen(false), [])

  useEffect(() => {
    setSelectedModelOverride(undefined)
    setSelectedEffortOverride(undefined)
    setSelectedFastModeOverride(undefined)
  }, [taskId])

  const refreshTasks = useCallback(() => {
    api.listTasks(10000).then(
      list => { setAllTasks(list) },
      () => {},
    )
  }, [api])

  const refreshInfoAndSettings = useCallback(() => {
    api.info().then(
      value => {
        setInfo(value)
      },
      (cause: unknown) => { setNotice(messageOf(cause)) },
    )
    api.getSettings().then(
      value => { setSettings(value) },
      () => {},
    )
  }, [api])

  useEffect(() => {
    let disposed = false
    void (async () => {
      try {
        const saved = localStorage.getItem('codex_workspaces')
        if (saved) {
          const paths: unknown = JSON.parse(saved)
          if (Array.isArray(paths)) {
            for (const path of paths) if (typeof path === 'string') {
              try { await api.addProject(path) } catch { /* Invalid legacy paths were demo entries. */ }
            }
          }
          localStorage.removeItem('codex_workspaces')
        }
        if (!disposed) await refreshProjects()
      } catch (cause) { if (!disposed) setNotice(messageOf(cause)) }
    })()
    return () => { disposed = true }
  }, [api, refreshProjects])

  const deleteSession = useCallback(async (id: string) => {
    const result = await api.deleteSession(id)
    setAllTasks(current => current.filter(task => !result.deleted.includes(task.task_id)))
    if (taskId && result.deleted.includes(taskId)) openTask(undefined)
    refreshTasks()
  }, [api, taskId, refreshTasks])

  const session = useSession(taskId, refreshTasks)
  const task = session.task
  const updateSession = useCallback(async (id: string, input: {title?: string; pinned?: boolean; archived?: boolean}) => {
    await api.updateSession(id, input)
    refreshTasks()
    if (id === taskId) session.reload()
  }, [api, refreshTasks, taskId, session.reload])
  useEffect(() => {
    if (!task) return
    changeSidebarMode(task.plugin_id || task.kind === 'chat' ? 'chat' : 'code')
    if (task.workspace) setActiveWorkspace(task.workspace)
  }, [task?.task_id, changeSidebarMode])

  useEffect(() => {
    const onHash = () => {
      if (isSettingsRoute()) {
        setSettingsOpen(true)
      } else {
        setTaskId(routeTaskId())
        setNotice(undefined)
      }
    }
    window.addEventListener('hashchange', onHash)
    return () => { window.removeEventListener('hashchange', onHash) }
  }, [])

  useEffect(() => {
    refreshInfoAndSettings()
    refreshTasks()
    const timer = window.setInterval(refreshTasks, 5000)
    return () => { window.clearInterval(timer) }
  }, [refreshInfoAndSettings, refreshTasks])

  const parentId = task?.parent_task_id ?? undefined
  useEffect(() => {
    let active = true
    setParent(undefined)
    if (parentId !== undefined) {
      api.getTask(parentId).then(value => { if (active) setParent(value) }, () => {})
    }
    return () => { active = false }
  }, [api, parentId])

  const openSettings = useCallback((tab: SettingsTab = 'models') => {
    setSettingsTab(tab)
    setSettingsOpen(true)
  }, [])

  const closeSettings = useCallback(() => {
    setSettingsOpen(false)
    if (location.pathname.endsWith('/settings')) {
      history.replaceState(null, '', '/agent' + (taskId === undefined ? '#/' : `#/tasks/${encodeURIComponent(taskId)}`))
    } else if (location.hash.startsWith('#/settings')) {
      location.hash = taskId === undefined ? '#/' : `#/tasks/${encodeURIComponent(taskId)}`
    }
    refreshInfoAndSettings()
  }, [refreshInfoAndSettings, taskId])

  const selectModel = useCallback(async (providerId: string, modelId: string): Promise<void> => {
    setSelectedModelOverride(modelId)
    try {
      const latest = settings ?? await api.getSettings()
      const capability = latest.providers.find(p => p.id === providerId)?.model_capabilities[modelId]
      const defaultEffort = capability?.default_effort ?? ''
      setSelectedEffortOverride(defaultEffort || null)
      setSelectedFastModeOverride(false)

      if (taskId !== undefined) {
        await api.updateTask(taskId, {
          model: modelId,
          provider: providerId,
          reasoning_effort: defaultEffort || null,
          fast_mode: false,
        })
        session.reload()
        refreshTasks()
      }

      const updated = await api.saveSettings({
        revision: latest.revision,
        providers: latest.providers.map(p => ({
          id: p.id,
          display_name: p.display_name ?? null,
          url: p.url,
          api_type: p.api_type,
          models: p.models,
          model_capabilities: p.model_capabilities,
        })),
        orchestrator_provider: providerId,
        orchestrator_model: modelId,
        reasoning_effort: defaultEffort,
        fast_mode: false,
        observer_enabled: latest.observer_enabled,
        observer_provider: latest.observer_provider,
        observer_model: latest.observer_model,
      })
      setSettings(updated)
      const latestInfo = await api.info()
      setInfo(latestInfo)
      setNotice(undefined)
    } catch (cause) {
      setNotice(messageOf(cause))
    }
  }, [api, refreshTasks, session.reload, settings, taskId])

  const setGenerationOptions = useCallback(async (reasoningEffort: string, fastMode: boolean): Promise<void> => {
    setSelectedEffortOverride(reasoningEffort || null)
    setSelectedFastModeOverride(fastMode)
    try {
      if (taskId !== undefined) {
        await api.updateTask(taskId, {
          reasoning_effort: reasoningEffort || null,
          fast_mode: fastMode,
        })
        session.reload()
      }
      const latest = settings ?? await api.getSettings()
      const updated = await api.saveSettings({
        revision: latest.revision,
        providers: latest.providers.map(p => ({
          id: p.id,
          display_name: p.display_name ?? null,
          url: p.url,
          api_type: p.api_type,
          models: p.models,
          model_capabilities: p.model_capabilities,
        })),
        orchestrator_provider: latest.orchestrator_provider,
        orchestrator_model: latest.orchestrator_model,
        reasoning_effort: reasoningEffort,
        fast_mode: fastMode,
        observer_enabled: latest.observer_enabled,
        observer_provider: latest.observer_provider,
        observer_model: latest.observer_model,
      })
      setSettings(updated)
      setNotice(undefined)
    } catch (cause) {
      setNotice(messageOf(cause))
    }
  }, [api, session.reload, settings, taskId])

  const tasks = allTasks.filter(item => item.parent_task_id === null)
  const ready = info?.ready === true
  const isChild = task?.parent_task_id != null
  const running = task?.status === 'running' || task?.status === 'cancelling'
  const heroPhase = taskId === undefined || (task?.status === 'draft' && session.events.length === 0)
  const composerNote = !ready && info !== undefined ? t('app.notReady') : notice ?? session.error

  const modelName = selectedModelOverride ?? task?.model ?? info?.model ?? settings?.orchestrator_model
  const reasoningEffort = selectedEffortOverride !== undefined
    ? selectedEffortOverride
    : (task?.reasoning_effort ?? settings?.reasoning_effort ?? '')
  const fastMode = selectedFastModeOverride !== undefined
    ? selectedFastModeOverride
    : (task?.fast_mode ?? settings?.fast_mode ?? false)

  const pendingSession=useRef<Promise<string>|null>(null)
  const uploadedAttachments=useRef(new Map<File,{task:string;receiptId:string}>())
  useEffect(()=>{pendingSession.current=null},[taskId,activeWorkspace,sidebarMode])
  useTaskNotifications(allTasks, taskId, openTask)
  const submit = useCallback(async (text: string, permissionMode: PermissionMode, attachment?:AttachmentSubmission): Promise<boolean> => {
    setNotice(undefined)
    requestNotificationPermission()
    try {
      const id = taskId ?? await (pendingSession.current??=api.createSession(activeWorkspace || info?.workspace, sidebarMode).catch(error=>{pendingSession.current=null;throw error}))
      const content=[]
      for(const file of attachment?.files??[]){
        let uploaded=uploadedAttachments.current.get(file)
        if(!uploaded||uploaded.task!==id){const value=await api.upload(id,file,attachment!.signal,p=>attachment!.onProgress(`正在上传 ${file.name} · ${p}%`));uploaded={task:id,receiptId:value.receiptId};uploadedAttachments.current.set(file,uploaded)}
        content.push({type:'file',receiptId:uploaded.receiptId})
      }
      if(attachment?.signal.aborted)throw Error('上传已取消')
      await api.prompt(id, text, {
        content,requestId:attachment?.requestId??crypto.randomUUID(),
        permission_mode: permissionMode,
        model: modelName ?? undefined,
        reasoning_effort: reasoningEffort ? reasoningEffort : undefined,
        fast_mode: fastMode,
      })
      uploadedAttachments.current.clear()
      refreshTasks()
      if (id === taskId) session.reload()
      else openTask(id)
      return true
    } catch (cause) {
      setNotice(messageOf(cause))
      return false
    }
  }, [api, activeWorkspace, info?.workspace, sidebarMode, fastMode, modelName, reasoningEffort, refreshTasks, session.reload, taskId])

  const stop = useCallback(() => {
    if (taskId === undefined) return
    api.cancel(taskId).then(
      () => { session.reload() },
      (cause: unknown) => { setNotice(messageOf(cause)) },
    )
  }, [api, session.reload, taskId])

  const interject = useCallback(async (text: string) => {
    if (taskId === undefined) throw new Error('NOT_RUNNING')
    await api.interject(taskId, text)
    session.reload()
  }, [api, session.reload, taskId])

  const interruptNode = useCallback((nodeId: string) => {
    if (taskId === undefined) return
    api.interruptNode(taskId, nodeId).then(
      () => { session.reload() },
      (cause: unknown) => { setNotice(messageOf(cause)) },
    )
  }, [api, session.reload, taskId])

  const saveSettings = useCallback((updated: SettingsData) => {
    setSettings(updated)
    refreshInfoAndSettings()
  }, [refreshInfoAndSettings])

  return {
    info, settings, tasks, allTasks, taskId, task, parent, parentId, session,
    ready, isChild, running, heroPhase, composerNote,
    modelName, reasoningEffort, fastMode,
    projects, workspaces, activeWorkspace: activeWorkspace || info?.workspace || '', selectWorkspace, addWorkspace, removeWorkspace, renameWorkspace,
    sidebarMode, changeSidebarMode, updateSession, deleteSession,
    folderDialogOpen, openFolderDialog, closeFolderDialog,
    canContinue: ready && task !== undefined && acceptsPrompt(task),
    settingsOpen, settingsTab, setSettingsTab,
    openSettings, closeSettings, saveSettings, refreshInfoAndSettings, selectModel, setGenerationOptions,
    openTask, submit, stop, interject, interruptNode,
  }
}

export type AgentWorkspace = ReturnType<typeof useAgentWorkspace>
