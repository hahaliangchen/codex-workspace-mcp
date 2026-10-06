import type { PermissionMode } from './api'
import { useCallback, useEffect, useState } from 'react'
import { acceptsPrompt, type AgentInfo, type SettingsData, type Task } from './api.ts'
import { useAgentApi } from './cordis/react.tsx'
import { t } from './i18n.ts'
import { useSession } from './useSession.ts'

export type SettingsTab = 'general' | 'models' | 'observer' | 'subagent' | 'plugins'

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

  const [workspaces, setWorkspaces] = useState<string[]>(() => {
    try {
      const saved = localStorage.getItem('codex_workspaces')
      if (saved) return JSON.parse(saved)
    } catch {}
    return [
      'd:\\enterpriseProject\\codex-workspace-mcp',
      'd:\\enterpriseProject\\pptx-editor-engine',
      'd:\\enterpriseProject\\deepseek-harness',
      'd:\\enterpriseProject\\woc-wnas',
      'd:\\enterpriseProject\\ai-ppt-server',
    ]
  })

  const [activeWorkspace, setActiveWorkspace] = useState<string>(() => {
    return localStorage.getItem('codex_active_workspace') || 'd:\\enterpriseProject\\codex-workspace-mcp'
  })

  const [folderDialogOpen, setFolderDialogOpen] = useState(false)

  const selectWorkspace = useCallback((ws: string) => {
    setActiveWorkspace(ws)
    try { localStorage.setItem('codex_active_workspace', ws) } catch {}
    openTask(undefined)
  }, [])

  const addWorkspace = useCallback((ws: string) => {
    const trimmed = ws.trim()
    if (!trimmed) return
    setWorkspaces((prev) => {
      const next = prev.includes(trimmed) ? prev : [trimmed, ...prev]
      try { localStorage.setItem('codex_workspaces', JSON.stringify(next)) } catch {}
      return next
    })
    setActiveWorkspace(trimmed)
    try { localStorage.setItem('codex_active_workspace', trimmed) } catch {}
    setFolderDialogOpen(false)
    openTask(undefined)
  }, [])

  const openFolderDialog = useCallback(() => setFolderDialogOpen(true), [])
  const closeFolderDialog = useCallback(() => setFolderDialogOpen(false), [])

  useEffect(() => {
    setSelectedModelOverride(undefined)
    setSelectedEffortOverride(undefined)
    setSelectedFastModeOverride(undefined)
  }, [taskId])

  const refreshTasks = useCallback(() => {
    api.listTasks(100).then(
      list => { setAllTasks(list) },
      () => {},
    )
  }, [api])

  const refreshInfoAndSettings = useCallback(() => {
    api.info().then(
      value => {
        setInfo(value)
        if (value.workspace) {
          setWorkspaces(prev => {
            if (!prev.includes(value.workspace)) {
              const next = [value.workspace, ...prev]
              try { localStorage.setItem('codex_workspaces', JSON.stringify(next)) } catch {}
              return next
            }
            return prev
          })
          if (!localStorage.getItem('codex_active_workspace')) {
            setActiveWorkspace(value.workspace)
          }
        }
      },
      (cause: unknown) => { setNotice(messageOf(cause)) },
    )
    api.getSettings().then(
      value => { setSettings(value) },
      () => {},
    )
  }, [api])

  const session = useSession(taskId, refreshTasks)
  const task = session.task

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
        expert_provider: latest.expert_provider,
        expert_model: latest.expert_model,
        enable_subagent: latest.enable_subagent,
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

  const toggleSubagent = useCallback(async (): Promise<void> => {
    try {
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
        reasoning_effort: latest.reasoning_effort ?? '',
        fast_mode: latest.fast_mode,
        expert_provider: latest.expert_provider,
        expert_model: latest.expert_model,
        enable_subagent: !latest.enable_subagent,
        observer_enabled: latest.observer_enabled,
        observer_provider: latest.observer_provider,
        observer_model: latest.observer_model,
      })
      setSettings(updated)
      const latestInfo = await api.info()
      setInfo(latestInfo)
    } catch (cause) {
      setNotice(messageOf(cause))
    }
  }, [api, settings])

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
        expert_provider: latest.expert_provider,
        expert_model: latest.expert_model,
        enable_subagent: latest.enable_subagent,
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
  const subagentEnabled = info?.subagent_enabled ?? settings?.enable_subagent ?? true

  const submit = useCallback(async (text: string, permissionMode: PermissionMode): Promise<boolean> => {
    setNotice(undefined)
    try {
      const id = taskId ?? await api.createSession()
      await api.prompt(id, text, {
        permission_mode: permissionMode,
        model: modelName ?? undefined,
        reasoning_effort: reasoningEffort ? reasoningEffort : undefined,
        fast_mode: fastMode,
      })
      refreshTasks()
      if (id === taskId) session.reload()
      else openTask(id)
      return true
    } catch (cause) {
      setNotice(messageOf(cause))
      return false
    }
  }, [api, fastMode, modelName, reasoningEffort, refreshTasks, session.reload, taskId])

  const stop = useCallback(() => {
    if (taskId === undefined) return
    api.cancel(taskId).then(
      () => { session.reload() },
      (cause: unknown) => { setNotice(messageOf(cause)) },
    )
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
    modelName, reasoningEffort, fastMode, subagentEnabled,
    workspaces, activeWorkspace, selectWorkspace, addWorkspace,
    folderDialogOpen, openFolderDialog, closeFolderDialog,
    canContinue: ready && task !== undefined && acceptsPrompt(task),
    settingsOpen, settingsTab, setSettingsTab,
    openSettings, closeSettings, saveSettings, refreshInfoAndSettings, selectModel, toggleSubagent, setGenerationOptions,
    openTask, submit, stop, interruptNode,
  }
}

export type AgentWorkspace = ReturnType<typeof useAgentWorkspace>
