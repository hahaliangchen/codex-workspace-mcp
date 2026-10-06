import { useEffect, useState } from 'react'
import { ConversationPage, HeroPage, type Tab } from './components/AgentPage.tsx'
import { CodexDock } from './components/CodexDock.tsx'
import { CodexRightPanel } from './components/CodexRightPanel.tsx'
import { SettingsModal } from './components/SettingsModal.tsx'
import { WorkspaceModal } from './components/WorkspaceModal.tsx'
import type { AgentSlotProps } from './cordis/slot-contract.ts'
import { toggleTheme } from './theme.ts'
import { useAgentWorkspace } from './useAgentWorkspace.ts'
import css from './App.module.css'

/** Assemble the task model and the Codex 4-column layout + SettingsModal overlay. */
export function App({ renderSlot }: AgentSlotProps) {
  const workspace = useAgentWorkspace()
  const [tab, setTab] = useState<Tab>('chat')
  const [dark, setDark] = useState(() => document.body.hasAttribute('data-ds-dark-theme'))
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false)
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const saved = localStorage.getItem('codex_sidebar_width')
    return saved ? Number(saved) : 260
  })
  const [rightPanelOpen, setRightPanelOpen] = useState(true)
  const [rightPanelWidth, setRightPanelWidth] = useState(() => {
    const saved = localStorage.getItem('codex_right_panel_width')
    return saved ? Math.max(300, Math.min(700, Number(saved))) : 380
  })
  const [newChatVersion, setNewChatVersion] = useState(0)

  const updateSidebarWidth = (w: number) => {
    setSidebarWidth(w)
    try { localStorage.setItem('codex_sidebar_width', String(w)) } catch {}
  }

  const updateRightPanelWidth = (w: number) => {
    setRightPanelWidth(w)
    try { localStorage.setItem('codex_right_panel_width', String(w)) } catch {}
  }

  useEffect(() => {
    const syncTheme = () => {
      setDark(document.body.hasAttribute('data-ds-dark-theme'))
    }
    const observer = new MutationObserver(syncTheme)
    observer.observe(document.body, { attributes: true, attributeFilter: ['data-ds-dark-theme'] })
    return () => { observer.disconnect() }
  }, [])

  const handleToggleCollapse = () => {
    setSidebarCollapsed(prev => !prev)
  }

  const handleNewChat = () => {
    setNewChatVersion(version => version + 1)
    setTab('chat')
    workspace.openTask(undefined)
  }

  const sidebar = renderSlot('agent.sidebar', {
    tasks: workspace.tasks,
    allTasks: workspace.allTasks,
    selectedId: workspace.isChild ? workspace.parentId : workspace.taskId,
    activeTaskId: workspace.taskId,
    workspace: workspace.activeWorkspace ?? workspace.info?.workspace ?? '',
    workspaces: workspace.workspaces,
    activeWorkspace: workspace.activeWorkspace,
    onSelectWorkspace: workspace.selectWorkspace,
    onAddWorkspace: workspace.openFolderDialog,
    dark,
    collapsed: sidebarCollapsed,
    width: sidebarWidth,
    onToggleCollapse: handleToggleCollapse,
    onNew: handleNewChat,
    onSelect: workspace.openTask,
    onToggleTheme: () => {
      toggleTheme()
      setDark(document.body.hasAttribute('data-ds-dark-theme'))
    },
    onOpenSettings: workspace.openSettings,
  })

  const actualSidebarWidth = sidebarCollapsed ? 0 : sidebarWidth

  return (
    <div className={css.appFrame} data-ds-dark-theme={dark ? '' : undefined}>
      {/* 1. Activity Dock (48px fixed on left) */}
      <CodexDock
        activeTab="home"
        dark={dark}
        onNewChat={handleNewChat}
        onOpenSettings={workspace.openSettings}
        onToggleTheme={() => {
          toggleTheme()
          setDark(document.body.hasAttribute('data-ds-dark-theme'))
        }}
      />

      {/* 2. Codex Sidebar (collapsible & resizable) */}
      {!sidebarCollapsed && (
        <div className={css.sidebarCol} style={{ width: actualSidebarWidth }}>
          {sidebar}
        </div>
      )}

      {/* Drag handle for Sidebar */}
      {!sidebarCollapsed && (
        <div
          className={css.dragHandle}
          onPointerDown={(e) => {
            e.preventDefault()
            const startX = e.clientX
            const startW = sidebarWidth
            const onMove = (moveEvt: PointerEvent) => {
              const nextW = Math.min(420, Math.max(200, startW + (moveEvt.clientX - startX)))
              updateSidebarWidth(nextW)
            }
            const onUp = () => {
              window.removeEventListener('pointermove', onMove)
              window.removeEventListener('pointerup', onUp)
            }
            window.addEventListener('pointermove', onMove)
            window.addEventListener('pointerup', onUp)
          }}
        />
      )}

      {/* 3. Center Main Column (Header + Conversation + Floating Composer) */}
      <div className={css.centerCol}>
        {workspace.heroPhase ? (
          <HeroPage key={newChatVersion} workspace={workspace} renderSlot={renderSlot} />
        ) : (
          <ConversationPage
            workspace={workspace}
            renderSlot={renderSlot}
            tab={tab}
            setTab={setTab}
            rightPanelOpen={rightPanelOpen}
            onToggleRightPanel={() => setRightPanelOpen(v => !v)}
          />
        )}
      </div>

      {/* Drag handle for Right Panel */}
      {rightPanelOpen && !workspace.heroPhase && (
        <div
          className={css.dragHandle}
          title="拖动调整面板宽度"
          onPointerDown={(e) => {
            e.preventDefault()
            const startX = e.clientX
            const startW = rightPanelWidth
            const onMove = (moveEvt: PointerEvent) => {
              const nextW = Math.min(700, Math.max(300, startW + (startX - moveEvt.clientX)))
              updateRightPanelWidth(nextW)
            }
            const onUp = () => {
              window.removeEventListener('pointermove', onMove)
              window.removeEventListener('pointerup', onUp)
            }
            window.addEventListener('pointermove', onMove)
            window.addEventListener('pointerup', onUp)
          }}
        />
      )}

      {/* 4. Right Inspector Panel (Changes, Sources) */}
      {rightPanelOpen && !workspace.heroPhase && (
        <CodexRightPanel
          width={rightPanelWidth}
          taskId={workspace.taskId}
          running={workspace.running}
          onChangesUpdated={workspace.session.reload}
          workspacePath={workspace.info?.workspace ?? 'codex-workspace-mcp'}
          events={workspace.session.events}
          onClose={() => setRightPanelOpen(false)}
          onOpenSettings={workspace.openSettings}
        />
      )}

      <SettingsModal
        open={workspace.settingsOpen}
        activeTab={workspace.settingsTab}
        workspacePath={workspace.info?.workspace ?? ''}
        onSelectTab={workspace.setSettingsTab}
        onClose={workspace.closeSettings}
        onThemeChanged={() => { setDark(document.body.hasAttribute('data-ds-dark-theme')) }}
        onSaved={workspace.refreshInfoAndSettings}
      />

      <WorkspaceModal
        open={workspace.folderDialogOpen}
        onClose={workspace.closeFolderDialog}
        onAddWorkspace={workspace.addWorkspace}
        existingWorkspaces={workspace.workspaces}
      />
    </div>
  )
}
