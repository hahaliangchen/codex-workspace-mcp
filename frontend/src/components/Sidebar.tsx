import { useMemo, useRef, useState } from 'react'
import clsx from 'clsx'
import {
  IconFolderCloseRegular,
  IconFolderOpenRegular,
} from '@deepseek-ai/dsh-client-ui-primitives'
import type { Task, TaskStatus } from '../api.ts'
import { t } from '../i18n.ts'
import css from './CodexSidebar.module.css'

export function dotState(status: TaskStatus): 'done' | 'warning' | 'ongoing' | 'error' | 'idle' {
  switch (status) {
    case 'running':
    case 'cancelling':
      return 'ongoing'
    case 'completed':
      return 'done'
    case 'failed':
    case 'interrupted':
      return 'error'
    case 'max_steps':
    case 'cancelled':
      return 'warning'
    case 'draft':
      return 'idle'
  }
}

export function workspaceName(path: string): string {
  const parts = path.replace(/^\\\\\?\\/, '').split(/[\\/]/).filter(Boolean)
  return parts.at(-1) ?? (path || 'codex-workspace-mcp')
}

export function taskTitle(task: Task): string {
  const line = task.prompt.trim().split('\n', 1)[0] ?? ''
  return line === '' ? t('app.untitled') : line
}

interface ProjectCategory {
  name: string
  items: string[]
}

const DEFAULT_PROJECT_GROUPS: ProjectCategory[] = [
  {
    name: 'bert simple',
    items: [
      '解释模型为何不归一化向量',
      '实施架构重构与泛化升级',
      '解释注意力机制中的V',
      '检查项目代码与训练数据',
    ],
  },
  {
    name: 'rag go',
    items: [
      '解决项目冲突并以远程为主',
      '测试 PDF 解析并记录耗时',
      '测试 RapidOCR PDF 识别速度',
      '查找 SQLite 的非占用用途',
      '我服了',
    ],
  },
  {
    name: 'RAG Milvus dev',
    items: [
      '分析断层日志事件存储',
      '添加 MinerU 配置验证脚本',
      '移除标签业务逻辑',
      '恢复 RAG 切片丢失全局视野',
      '修复 sing-box DNS 配置',
    ],
  },
  {
    name: 'woc-wnas',
    items: [
      '把 master 分支合并到 master-bcl',
    ],
  },
]

export function Sidebar({
  tasks,
  allTasks,
  selectedId,
  activeTaskId,
  workspace,
  workspaces = [],
  activeWorkspace,
  onSelectWorkspace,
  onAddWorkspace,
  dark: _dark,
  collapsed = false,
  width = 260,
  onToggleCollapse,
  onNew,
  onSelect,
  onToggleTheme: _onToggleTheme,
  onOpenSettings,
}: {
  tasks: readonly Task[]
  allTasks?: readonly Task[] | undefined
  selectedId: string | undefined
  activeTaskId?: string | undefined
  workspace: string
  workspaces?: readonly string[] | undefined
  activeWorkspace?: string | undefined
  onSelectWorkspace?: ((ws: string) => void) | undefined
  onAddWorkspace?: (() => void) | undefined
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
  const [searchQuery, setSearchQuery] = useState('')
  const [folderOpen, setFolderOpen] = useState(true)
  const [expandedProjects, setExpandedProjects] = useState<Record<string, boolean>>({
    'bert simple': true,
    'rag go': true,
    'RAG Milvus dev': true,
    'woc-wnas': true,
  })
  const [showMoreProjects, setShowMoreProjects] = useState(false)
  const searchInputRef = useRef<HTMLInputElement>(null)

  const currentHighlightedId = activeTaskId ?? selectedId

  // Build children map
  const childrenByParent = useMemo(() => {
    const map = new Map<string, Task[]>()
    for (const item of allTasks ?? []) {
      if (item.parent_task_id !== null) {
        const list = map.get(item.parent_task_id) ?? []
        list.push(item)
        map.set(item.parent_task_id, list)
      }
    }
    return map
  }, [allTasks])

  // Filter tasks based on search
  const filteredTasks = useMemo(() => {
    const q = searchQuery.trim().toLowerCase()
    if (q === '') return tasks
    return tasks.filter((item) => {
      if (taskTitle(item).toLowerCase().includes(q)) return true
      const children = childrenByParent.get(item.task_id) ?? []
      return children.some(child => taskTitle(child).toLowerCase().includes(q))
    })
  }, [tasks, searchQuery, childrenByParent])

  // Map titles to task ids for quick lookup
  const taskByTitle = useMemo(() => {
    const map = new Map<string, Task>()
    for (const t of allTasks ?? tasks) {
      map.set(taskTitle(t).trim(), t)
    }
    return map
  }, [allTasks, tasks])

  const toggleProject = (name: string) => {
    setExpandedProjects(prev => ({ ...prev, [name]: !prev[name] }))
  }

  if (collapsed) {
    return (
      <div className={clsx(css.root, css.collapsedRoot)} style={{ width: 56 }}>
        <button
          type="button"
          className={css.iconBtn}
          title="展开侧边栏"
          onClick={onToggleCollapse}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
            <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
            <line x1="9" x2="9" y1="3" y2="21" />
          </svg>
        </button>
      </div>
    )
  }

  return (
    <div className={css.root} style={{ width }}>
      {/* Static product identity; new conversations use the dedicated button. */}
      <div className={css.header}>
        <div className={css.brandGroup} title="Composer">
          <span className={css.brandTitle}>Composer</span>
        </div>

        <div className={css.headerActions}>
          <button
            type="button"
            className={css.iconBtn}
            title="通知"
            onClick={onOpenSettings}
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
              <path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" />
              <path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" />
            </svg>
          </button>

          <button
            type="button"
            className={css.iconBtn}
            title="搜索 (Ctrl+K)"
            onClick={() => {
              setSearchOpen(!searchOpen)
              if (!searchOpen) window.setTimeout(() => searchInputRef.current?.focus(), 50)
            }}
          >
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
              <circle cx="11" cy="11" r="8" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>
          </button>

          {onToggleCollapse && (
            <button
              type="button"
              className={css.iconBtn}
              title="折叠侧边栏"
              onClick={onToggleCollapse}
            >
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8">
                <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
                <line x1="9" x2="9" y1="3" y2="21" />
              </svg>
            </button>
          )}
        </div>
      </div>

      {/* 2. New Chat Button: 新聊天 */}
      <div className={css.newChatRow}>
        <button
          type="button"
          className={css.newChatBtn}
          onClick={onNew}
        >
          <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
            <path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7" />
            <path d="M18.375 2.625a2.121 2.121 0 1 1 3 3L12 15l-4 1 1-4Z" />
          </svg>
          <span>新聊天</span>
        </button>
      </div>

      {/* 3. Search Bar Input when open */}
      {searchOpen && (
        <div className={css.searchBox}>
          <input
            ref={searchInputRef}
            type="text"
            className={css.searchInput}
            placeholder="搜索任务或会话..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape') {
                setSearchQuery('')
                setSearchOpen(false)
              }
            }}
          />
        </div>
      )}

      {/* 4. Scrollable Areas: 项目 (Projects) & 最近 (Recent) */}
      <div className={css.scrollArea}>
        {/* Section: 项目 */}
        <div className={css.section}>
          <div className={css.sectionHeaderRow}>
            <div className={css.sectionTitleLeft}>
              <span>项目</span>
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round" strokeLinejoin="round" className={css.sectionChevron}>
                <polyline points="6 9 12 15 18 9" />
              </svg>
            </div>
            <div className={css.sectionActionsRight}>
              <button type="button" className={css.sectionActionBtn} title="更多选项">
                <svg width="14" height="14" viewBox="0 0 24 24" fill="currentColor">
                  <circle cx="5" cy="12" r="1.8" />
                  <circle cx="12" cy="12" r="1.8" />
                  <circle cx="19" cy="12" r="1.8" />
                </svg>
              </button>
              <button
                type="button"
                className={css.sectionActionBtn}
                title="选择文件夹 / 添加项目"
                onClick={onAddWorkspace}
              >
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round">
                  <line x1="12" y1="5" x2="12" y2="19" />
                  <line x1="5" y1="12" x2="19" y2="12" />
                </svg>
              </button>
            </div>
          </div>

          {/* Active Primary Workspace */}
          {(() => {
            const currentWs = activeWorkspace || workspace
            return (
              <div className={css.projectGroup}>
                <div
                  className={clsx(css.projectHeaderRow, css.projectHeaderRowActive)}
                  onClick={() => {
                    setFolderOpen(!folderOpen)
                    onSelectWorkspace?.(currentWs)
                  }}
                  title={currentWs}
                >
                  <span className={css.folderIcon}>
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                      <path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.93a2 2 0 0 1-1.66-.9l-.82-1.2A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2Z" />
                    </svg>
                  </span>
                  <span className={css.projectTitle}>{workspaceName(currentWs)}</span>
                </div>

            {folderOpen && (
              <div className={css.projectSubList}>
                {filteredTasks.length === 0 ? (
                  <button
                    type="button"
                    className={clsx(css.sessionRow, css.sessionActive)}
                    onClick={onNew}
                  >
                    <span className={css.sessionTitle}>Clarify request</span>
                  </button>
                ) : (
                  filteredTasks.map((item) => {
                    const isSelected = item.task_id === currentHighlightedId
                    const isRunning = item.status === 'running' || item.status === 'cancelling'

                    return (
                      <button
                        key={item.task_id}
                        type="button"
                        className={clsx(css.sessionRow, isSelected && css.sessionActive)}
                        onClick={() => onSelect(item.task_id)}
                        title={taskTitle(item)}
                      >
                        <span className={css.sessionTitle}>{taskTitle(item)}</span>
                        {isRunning && (
                          <span className={css.runningSpinner} title="执行中">
                            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
                              <path d="M21 12a9 9 0 1 1-6.219-8.56" />
                            </svg>
                          </span>
                        )}
                      </button>
                    )
                  })
                )}
              </div>
            )}
          </div>
        )
      })()}
          {/* Other Registered Workspaces from Workspaces List */}
          {workspaces
            .filter(ws => ws !== (activeWorkspace || workspace))
            .map((ws) => {
              const name = workspaceName(ws)
              return (
                <div key={ws} className={css.projectGroup}>
                  <div
                    className={css.projectHeaderRow}
                    onClick={() => onSelectWorkspace?.(ws)}
                    title={ws}
                  >
                    <span className={css.folderIcon}>
                      <IconFolderCloseRegular size={14} />
                    </span>
                    <span className={css.projectTitle}>{name}</span>
                  </div>
                </div>
              )
            })}

          {/* Other Project Groups Matching User Screenshot */}
          {DEFAULT_PROJECT_GROUPS.map((proj) => {
            const isOpen = expandedProjects[proj.name] !== false
            return (
              <div key={proj.name} className={css.projectGroup}>
                <div
                  className={css.projectHeaderRow}
                  onClick={() => toggleProject(proj.name)}
                >
                  <span className={css.folderIcon}>
                    {isOpen ? <IconFolderOpenRegular size={14} /> : <IconFolderCloseRegular size={14} />}
                  </span>
                  <span className={css.projectTitle}>{proj.name}</span>
                </div>

                {isOpen && (
                  <div className={css.projectSubList}>
                    {proj.items.map((title) => {
                      const matchedTask = taskByTitle.get(title)
                      const isSelected = matchedTask ? matchedTask.task_id === currentHighlightedId : false
                      const isRunning = matchedTask ? matchedTask.status === 'running' : false

                      return (
                        <button
                          key={title}
                          type="button"
                          className={clsx(css.sessionRow, isSelected && css.sessionActive)}
                          onClick={() => {
                            if (matchedTask) onSelect(matchedTask.task_id)
                          }}
                          title={title}
                        >
                          <span className={css.sessionTitle}>{title}</span>
                          {isRunning && (
                            <span className={css.runningSpinner} title="执行中">
                              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
                                <path d="M21 12a9 9 0 1 1-6.219-8.56" />
                              </svg>
                            </span>
                          )}
                        </button>
                      )
                    })}
                  </div>
                )}
              </div>
            )
          })}

          <button
            type="button"
            className={css.expandBtn}
            onClick={() => setShowMoreProjects(!showMoreProjects)}
          >
            <span>{showMoreProjects ? '收起显示' : '展开显示'}</span>
          </button>
        </div>

        {/* Section: 最近 */}
        <div className={css.section} style={{ marginTop: 14 }}>
          <div className={css.sectionHeader}>最近</div>

          <div style={{ display: 'flex', flexDirection: 'column', gap: 1 }}>
            {tasks.length > 0 ? (
              tasks.slice(0, 5).map((task) => {
                const isSelected = task.task_id === currentHighlightedId
                return (
                  <button
                    key={task.task_id}
                    type="button"
                    className={clsx(css.recentRow, isSelected && css.recentRowActive)}
                    onClick={() => onSelect(task.task_id)}
                    title={taskTitle(task)}
                  >
                    <span className={css.sessionTitle}>{taskTitle(task)}</span>
                  </button>
                )
              })
            ) : (
              [
                'Clarify request',
                '移植 DSH 核心功能到 Rust',
                '解释模型为何不归一化向量',
              ].map((title) => (
                <button
                  key={title}
                  type="button"
                  className={css.recentRow}
                  onClick={onNew}
                  title={title}
                >
                  <span className={css.sessionTitle}>{title}</span>
                </button>
              ))
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
