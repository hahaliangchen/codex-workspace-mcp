import { useEffect, useLayoutEffect, useMemo, useRef, type Dispatch, type SetStateAction } from 'react'
import clsx from 'clsx'
import { useBrandIcon, usePalette } from '../brand.ts'
import { taskTitle, workspaceName } from './Sidebar.tsx'
import { TaskBoardCard } from './TaskBoardCard.tsx'
import { FlowView } from './FlowView.tsx'
import { latestContextUsage } from './ContextRing.tsx'
import { SkillPagesView } from './SkillPagesView.tsx'
import type { AgentSlotProps } from '../cordis/slot-contract.ts'
import root from '../dsh/conversation/ConversationRoot.module.css'
import welcomeCss from './Welcome.module.css'
import readonlyCss from '../dsh/subagent/SubagentReadOnlyComposer.module.css'
import { t } from '../i18n.ts'
import { formatDuration } from '../model.ts'
import { useTaskDuration } from '../useTaskDuration.ts'
import type { AgentWorkspace } from '../useAgentWorkspace.ts'
import css from '../App.module.css'
import headerCss from './CodexHeader.module.css'

interface PageProps {
  readonly workspace: AgentWorkspace
  readonly renderSlot: AgentSlotProps['renderSlot']
}

export function HeroPage({ workspace, renderSlot }: PageProps) {
  const currentWs = workspace.projects.find(project => project.path === workspace.activeWorkspace)?.path ?? ''
  const wsName = workspaceName(currentWs)
  const logoImg = useBrandIcon()
  // 新芽 plants, 银杏 harvests: the greeting follows the season of the theme.
  const verb = usePalette() === 'ginkgo' ? '收获' : '种'

  return (
    <main className={welcomeCss.page} data-phase="hero">
      <div className={welcomeCss.welcome}>
        <div className={welcomeCss.heading}>
          <img src={logoImg} className={welcomeCss.logo} alt="" />
          <h1 className={welcomeCss.title}>
            {workspace.sidebarMode === 'chat' ? '今天想聊点什么？' : !currentWs ? '今天想一起做点什么？' : <>今天想在 <span className={welcomeCss.workspace}>{wsName}</span> 里{verb}点什么？</>}
          </h1>
        </div>
        {renderSlot('agent.composer', {
          variant: 'hero',
          disabled: !workspace.ready,
          note: workspace.composerNote,
          running: false,
          workspacePath: currentWs,
          workspaces: workspace.workspaces,
          activeWorkspace: currentWs,
          onSelectWorkspace: workspace.selectWorkspace,
          onOpenAddWorkspace: workspace.openFolderDialog,
          modelName: workspace.modelName ?? undefined,
          reasoningEffort: workspace.reasoningEffort ?? undefined,
          fastMode: workspace.fastMode,

          settings: workspace.settings,
          onSelectModel: workspace.selectModel,
          onSetGenerationOptions: workspace.setGenerationOptions,
          onOpenSettings: workspace.openSettings,
          onSubmit: workspace.submit,
          onStop: undefined,
        })}
      </div>
    </main>
  )
}

export function SkillPackagesPage({ workspace, onBack }: { workspace: AgentWorkspace; onBack: () => void }) {
  return <main className={root.root} style={{ minHeight: 0 }}>
    <header className={headerCss.header}><strong>技能页面</strong><button onClick={onBack}>返回对话</button></header>
    <SkillPagesView workspaceRoot={workspace.activeWorkspace ?? workspace.info?.workspace ?? ''} />
  </main>
}

export type Tab = 'chat' | 'flow' | 'trajectory' | 'skill-pages'

export function ConversationPage({
  workspace,
  renderSlot,
  tab,
  setTab,
  rightPanelOpen = true,
  onToggleRightPanel,
}: PageProps & {
  readonly tab: Tab
  readonly setTab: Dispatch<SetStateAction<Tab>>
  readonly rightPanelOpen?: boolean | undefined
  readonly onToggleRightPanel?: (() => void) | undefined
}) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const followTail = useRef(true)
  useEffect(() => { followTail.current = true }, [workspace.taskId, tab])
  useLayoutEffect(() => {
    const scroll = scrollRef.current
    if (scroll !== null && followTail.current && tab === 'chat') scroll.scrollTop = scroll.scrollHeight
  }, [workspace.session.events, tab])
  useEffect(() => {
    const scroll = scrollRef.current
    const transcript = scroll?.firstElementChild
    if (!scroll || !transcript || tab !== 'chat') return
    // Streaming text grows between event batches; follow the rendered size
    // rather than jumping only when a network event arrives.
    const observer = new ResizeObserver(() => {
      if (followTail.current) scroll.scrollTop = scroll.scrollHeight
    })
    observer.observe(transcript)
    return () => observer.disconnect()
  }, [workspace.taskId, tab])

  const {
    task, parent, parentId, isChild, running, ready, canContinue,
    composerNote, session, openTask, submit, stop, interruptNode,
  } = workspace
  const durationInfo = useTaskDuration(session.events, task, running)
  const contextUsage = useMemo(() => latestContextUsage(session.events), [session.events])
  const currentTitle = task === undefined ? t('app.untitled') : taskTitle(task)

  return (
    <main className={root.root} data-phase="active" style={{ background: 'var(--codex-bg-main, #ffffff)' }}>
      {/* Codex-style Top Header: [ 📄 Clarify request ]  [ 对话 | 流程图 | 轨迹 ]  [ 耗时 12s | ⧉ | ... ] */}
      <header className={headerCss.header}>
        {/* Left: Document icon + Session title */}
        <div className={headerCss.titleArea}>
          <span className={headerCss.titleIcon}>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
              <path d="M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7.5L14.5 2z" />
              <polyline points="14 2 14 8 20 8" />
            </svg>
          </span>
          <span className={headerCss.titleText} title={currentTitle}>
            {isChild && parent ? `${taskTitle(parent)} / ` : ''}{currentTitle}
          </span>
        </div>

        {/* Center: Clean Segmented Pill Tabs */}
        <div className={headerCss.tabPillGroup} role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'chat'}
            className={clsx(headerCss.tabPill, tab === 'chat' && headerCss.tabPillActive)}
            onClick={() => setTab('chat')}
          >
            对话
          </button>
          {/* 任务流节点展示暂时隐藏：新的虚函数执行路径跑通后再按新设计恢复。 */}
          <button type="button" role="tab" aria-selected={tab === 'skill-pages'} className={clsx(headerCss.tabPill, tab === 'skill-pages' && headerCss.tabPillActive)} onClick={() => setTab('skill-pages')}>技能页面</button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'trajectory'}
            className={clsx(headerCss.tabPill, tab === 'trajectory' && headerCss.tabPillActive)}
            onClick={() => setTab('trajectory')}
          >
            执行轨迹
          </button>
        </div>

        {/* Right: Duration badge, Right Panel Toggle */}
        <div className={headerCss.rightUtils}>
          {durationInfo.currentTurnDurationMs !== undefined && (
            <div className={headerCss.durationBadge}>
              {durationInfo.isRunning && <span className={headerCss.liveDot} />}
              <span>{durationInfo.isRunning ? '进行中' : '耗时'} {formatDuration(durationInfo.currentTurnDurationMs)}</span>
            </div>
          )}

          {/* Toggle Right Inspector Panel (Matches split icon in screenshot) */}
          {onToggleRightPanel && (
            <button
              type="button"
              className={clsx(headerCss.iconBtn, rightPanelOpen && headerCss.iconBtnActive)}
              title={rightPanelOpen ? '隐藏右侧检查器' : '显示右侧检查器'}
              onClick={onToggleRightPanel}
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2">
                <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
                <line x1="15" x2="15" y1="3" y2="21" />
              </svg>
            </button>
          )}

        </div>
      </header>

      <div className={root.body}>
        <div
          ref={scrollRef}
          className={root.scrollBody}
          data-conversation-scroll
          onScroll={(event) => {
            const box = event.currentTarget
            followTail.current = box.scrollHeight - box.scrollTop - box.clientHeight < 80
          }}
        >
          <div className={css.session} data-slot="conversation.session" style={tab === 'flow' || tab === 'skill-pages' ? { height: '100%', flex: 1, display: 'flex', flexDirection: 'column' } : undefined}>
            <div className={root.viewArea} style={tab === 'flow' || tab === 'skill-pages' ? { height: '100%', flex: 1, minHeight: 520, display: 'flex' } : undefined}>
              {tab === 'chat'
                ? <>{renderSlot('agent.chat', { events: session.events, task, running, onOpenTask: openTask, onChangesUpdated: session.reload })}{task && !isChild && <TaskBoardCard task={task} reload={session.reload} />}</>
                : tab === 'skill-pages'
                ? <SkillPagesView workspaceRoot={workspace.activeWorkspace ?? workspace.info?.workspace ?? ''} />
                : tab === 'flow'
                ? <FlowView key={workspace.taskId} events={session.events} task={task} running={running} onInterruptNode={interruptNode} />
                : renderSlot('agent.trajectory', { events: session.events, onOpenTask: openTask })}
            </div>
          </div>
          <div className={root.composerSeat}>
            {isChild
              ? (
                  <div className={readonlyCss.frame}>
                    <span>{t('app.childReadonly')}</span>
                    <button
                      type="button"
                      className={css.readonlyAction}
                      onClick={() => { openTask(parentId) }}
                    >
                      {t('app.backToParent')}
                    </button>
                  </div>
                )
              : renderSlot('agent.composer', {
                  variant: 'composer',
                  // A running turn still takes messages: interjections or a queue.
                  disabled: !ready || (!canContinue && !running),
                  note: composerNote,
                  running,
                  workspacePath: workspace.activeWorkspace ?? workspace.info?.workspace,
                  workspaces: workspace.workspaces,
                  activeWorkspace: workspace.activeWorkspace,
                  onSelectWorkspace: workspace.selectWorkspace,
                  onOpenAddWorkspace: workspace.openFolderDialog,
                  modelName: workspace.modelName ?? undefined,
                  reasoningEffort: workspace.reasoningEffort ?? undefined,
                  fastMode: workspace.fastMode,

                  settings: workspace.settings,
                  onSelectModel: workspace.selectModel,
                  onSetGenerationOptions: workspace.setGenerationOptions,
                  onOpenSettings: workspace.openSettings,
                  onSubmit: submit,
                  onStop: stop,
                  onInterject: workspace.interject,
                  contextUsage,
                })}
          </div>
        </div>
      </div>
    </main>
  )
}
