import { useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react'
import { createPortal } from 'react-dom'
import clsx from 'clsx'
import {
  IconCheckOutlineRegular,
  IconChevronDownOutlineRegular,
  IconChevronLeftOutlineRegular,
  IconChevronRightOutlineRegular,
  IconFolderOpenRegular,
  IconFolderCloseRegular,
  IconPlusOutlineRegular,
  RiskConfirmation,
  Switch,
  Tooltip,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { Menu } from '../dsh/ui-primitives/Menu.tsx'
import heroShellCss from '../dsh/conversation/HeroShell.module.css'
import type { SettingsData } from '../api.ts'
import { useAgentApi } from '../cordis/react.tsx'
import modelCss from '../dsh/model-selection/ModelSelect.module.css'
import { t } from '../i18n.ts'
import type { SettingsTab } from '../useAgentWorkspace.ts'
import { workspaceName } from './Sidebar.tsx'
import local from './Composer.module.css'
import codexCss from './CodexComposer.module.css'

const STOP_ICON = (
  <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden>
    <rect x="3" y="3" width="10" height="10" rx="3" fill="currentColor" />
  </svg>
)

interface ActiveToken {
  kind: '@' | '/'
  query: string
  start: number
}

interface Suggestion {
  id: string
  label: string
  detail: string
  insert: string
}

function activeToken(value: string, caret: number): ActiveToken | null {
  const match = /(^|\s)([@/])([^\s]*)$/.exec(value.slice(0, caret))
  const kind = match?.[2]
  if (match === null || (kind !== '@' && kind !== '/')) return null
  return { kind, query: match[3] ?? '', start: match.index + (match[1]?.length ?? 0) }
}

function mentionInsert(path: string): string {
  return path.includes(' ') ? `@"${path}" ` : `@${path} `
}

export type DshPermissionMode = 'read_only' | 'workspace_write' | 'full_access'

export function Composer({
  variant,
  disabled,
  note,
  running,
  workspacePath = '',
  workspaces = [],
  activeWorkspace,
  onSelectWorkspace,
  onOpenAddWorkspace,
  modelName = '',
  reasoningEffort,
  fastMode = false,
  subagentEnabled = false,
  settings,
  onSelectModel,
  onSetGenerationOptions,
  onToggleSubagent,
  onOpenSettings,
  onSubmit,
  onStop,
}: {
  variant: 'hero' | 'composer'
  disabled: boolean
  note: string | undefined
  running: boolean
  workspacePath?: string | undefined
  workspaces?: readonly string[] | undefined
  activeWorkspace?: string | undefined
  onSelectWorkspace?: ((ws: string) => void) | undefined
  onOpenAddWorkspace?: (() => void) | undefined
  modelName?: string | undefined
  reasoningEffort?: string | null | undefined
  fastMode?: boolean | undefined
  subagentEnabled?: boolean | undefined
  settings?: SettingsData | undefined
  onSelectModel?: ((providerId: string, modelId: string) => Promise<void>) | undefined
  onSetGenerationOptions?: ((reasoningEffort: string, fastMode: boolean) => Promise<void>) | undefined
  onToggleSubagent?: (() => Promise<void>) | undefined
  onOpenSettings?: ((tab?: SettingsTab) => void) | undefined
  onSubmit: (text: string, permissionMode: DshPermissionMode) => Promise<boolean>
  onStop: (() => void) | undefined
}) {
  const [draft, setDraft] = useState('')
  const [submitting, setSubmitting] = useState(false)
  const [workspaceMenuOpen, setWorkspaceMenuOpen] = useState(false)
  const workspaceAnchorRef = useRef<HTMLButtonElement>(null)
  const [permissionMode, setPermissionMode] = useState<DshPermissionMode>(() => {
    try {
      const saved = localStorage.getItem('dsh_permission_mode')
      if (saved === 'read_only' || saved === 'workspace_write' || saved === 'full_access') {
        return saved
      }
    } catch {}
    return 'full_access'
  })
  const [permMenuOpen, setPermMenuOpen] = useState(false)
  const [permMenuPos, setPermMenuPos] = useState<{ top?: number; bottom?: number; left: number }>({ left: 16 })
  const permTriggerRef = useRef<HTMLButtonElement>(null)
  const permMenuRef = useRef<HTMLDivElement>(null)
  const [showRiskConfirm, setShowRiskConfirm] = useState(false)
  const [riskAcknowledged, setRiskAcknowledged] = useState(false)
  const [bannerDismissed, setBannerDismissed] = useState(() => {
    try {
      return localStorage.getItem('codex_perm_banner_dismissed') === 'true'
    } catch {
      return false
    }
  })
  const [modelMenuOpen, setModelMenuOpen] = useState(false)
  const [pane, setPane] = useState<'root' | 'model' | 'effort'>('root')
  const [menuPos, setMenuPos] = useState<{ top?: number; bottom?: number; right: number }>({ right: 24 })
  const api = useAgentApi()
  const [caret, setCaret] = useState(0)
  const [commands, setCommands] = useState<readonly { name: string; description: string }[]>([])
  const [files, setFiles] = useState<readonly { path: string; kind: 'file' | 'dir' }[]>([])
  const [menuIndex, setMenuIndex] = useState(0)
  const [dismissedKey, setDismissedKey] = useState('')
  const inputRef = useRef<HTMLTextAreaElement>(null)
  const modelTriggerRef = useRef<HTMLButtonElement>(null)
  const modelMenuRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (variant === 'hero' && !disabled) inputRef.current?.focus()
  }, [variant, disabled])

  const canSend = !disabled && !running && !submitting && draft.trim() !== ''
  const token = useMemo(() => activeToken(draft, caret), [draft, caret])
  const tokenKey = token === null ? '' : `${token.kind}:${token.start}:${token.query}`

  useEffect(() => {
    api.commands().then(
      (body) => { setCommands(body.commands) },
      () => {},
    )
  }, [api])

  useEffect(() => {
    if (token?.kind !== '@') return
    const query = token.query
    const timer = window.setTimeout(() => {
      api.files(query).then(
        (body) => { setFiles(body.files) },
        () => { setFiles([]) },
      )
    }, 120)
    return () => { window.clearTimeout(timer) }
  }, [api, token?.kind, token?.query])

  const suggestions = useMemo((): readonly Suggestion[] => {
    if (token?.kind === '@') {
      return files.map((file) => ({
        id: file.path,
        label: file.path,
        detail: file.kind === 'dir' ? '目录' : '文件',
        insert: mentionInsert(file.path),
      }))
    }
    if (token?.kind === '/') {
      const query = token.query.toLowerCase()
      return commands
        .filter((command) => query === '' || command.name.toLowerCase().includes(query))
        .slice(0, 20)
        .map((command) => ({
          id: command.name,
          label: `/${command.name}`,
          detail: command.description,
          insert: `/${command.name} `,
        }))
    }
    return []
  }, [token, files, commands])
  const menuOpen = token !== null && dismissedKey !== tokenKey && suggestions.length > 0
  const activeIndex = Math.min(menuIndex, Math.max(suggestions.length - 1, 0))

  const syncCaret = () => { setCaret(inputRef.current?.selectionStart ?? draft.length) }

  const applySuggestion = (suggestion: Suggestion) => {
    if (token === null) return
    const next = `${draft.slice(0, token.start)}${suggestion.insert}${draft.slice(caret)}`
    const pos = token.start + suggestion.insert.length
    setDraft(next)
    setMenuIndex(0)
    window.requestAnimationFrame(() => {
      inputRef.current?.focus()
      inputRef.current?.setSelectionRange(pos, pos)
      setCaret(pos)
    })
  }

  useLayoutEffect(() => {
    const input = inputRef.current
    if (input === null) return
    input.style.height = 'auto'
    input.style.height = `${input.scrollHeight}px`
  }, [draft])

  useEffect(() => {
    if (!modelMenuOpen) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node | null
      if (
        target !== null
        && !modelTriggerRef.current?.contains(target)
        && !modelMenuRef.current?.contains(target)
      ) {
        setModelMenuOpen(false)
      }
    }
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') setModelMenuOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    window.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      window.removeEventListener('keydown', onKeyDown)
    }
  }, [modelMenuOpen])

  useEffect(() => {
    if (!permMenuOpen) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node | null
      if (
        target !== null
        && !permTriggerRef.current?.contains(target)
        && !permMenuRef.current?.contains(target)
      ) {
        setPermMenuOpen(false)
      }
    }
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') setPermMenuOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    window.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      window.removeEventListener('keydown', onKeyDown)
    }
  }, [permMenuOpen])

  const togglePermDropdown = () => {
    if (permMenuOpen) {
      setPermMenuOpen(false)
      return
    }
    const rect = permTriggerRef.current?.getBoundingClientRect()
    if (rect !== undefined) {
      const left = Math.max(16, rect.left)
      if (rect.top > 320) {
        setPermMenuPos({ bottom: window.innerHeight - rect.top + 8, left })
      } else {
        setPermMenuPos({ top: rect.bottom + 8, left })
      }
    }
    setPermMenuOpen(true)
  }

  const selectPermissionMode = (mode: DshPermissionMode) => {
    setPermissionMode(mode)
    setPermMenuOpen(false)
    try {
      localStorage.setItem('dsh_permission_mode', mode)
    } catch {}
  }

  const handleSelectPermission = (mode: DshPermissionMode) => {
    setPermMenuOpen(false)
    if (mode === 'full_access' && permissionMode !== 'full_access') {
      setRiskAcknowledged(false)
      setShowRiskConfirm(true)
      return
    }
    selectPermissionMode(mode)
  }

  const openModelDropdown = () => {
    if (modelMenuOpen) {
      setModelMenuOpen(false)
      return
    }
    setPane('root')
    const rect = modelTriggerRef.current?.getBoundingClientRect()
    if (rect !== undefined) {
      const right = Math.max(16, window.innerWidth - rect.right)
      if (rect.top > 440) {
        setMenuPos({ bottom: window.innerHeight - rect.top + 8, right })
      } else {
        setMenuPos({ top: rect.bottom + 8, right })
      }
    }
    setModelMenuOpen(true)
  }

  const submit = async () => {
    if (!canSend) return
    setSubmitting(true)
    try {
      if (await onSubmit(draft.trim(), permissionMode)) setDraft('')
    } finally {
      setSubmitting(false)
    }
  }

  const onKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (menuOpen) {
      if (event.key === 'ArrowDown') {
        event.preventDefault()
        setMenuIndex((index) => (index + 1) % suggestions.length)
        return
      }
      if (event.key === 'ArrowUp') {
        event.preventDefault()
        setMenuIndex((index) => (index - 1 + suggestions.length) % suggestions.length)
        return
      }
      if (event.key === 'Escape') {
        event.preventDefault()
        setDismissedKey(tokenKey)
        return
      }
      if ((event.key === 'Enter' || event.key === 'Tab') && !event.nativeEvent.isComposing) {
        event.preventDefault()
        const suggestion = suggestions[activeIndex]
        if (suggestion !== undefined) applySuggestion(suggestion)
        return
      }
    }
    if (event.key === 'Enter' && !event.shiftKey && !event.nativeEvent.isComposing) {
      event.preventDefault()
      void submit()
    }
  }

  const wsName = workspaceName(activeWorkspace || workspacePath || '')
  const placeholder = t(variant === 'hero' ? 'placeholder.hero' : 'placeholder.default')
  const activeModelName = modelName || settings?.orchestrator_model || ''
  const displayModel = activeModelName || t('app.configureModel')
  const providers = settings?.providers ?? []

  const activeProvider = providers.find(p => p.models.includes(activeModelName))
    ?? providers.find(p => p.id === settings?.orchestrator_provider)
  const selectedCapability = (activeProvider && activeModelName ? activeProvider.model_capabilities[activeModelName] : undefined)
    ?? providers.find(p => p.id === settings?.orchestrator_provider)?.model_capabilities[settings?.orchestrator_model ?? '']

  const supportedEfforts = selectedCapability?.reasoning_efforts ?? []
  const currentEffort = (reasoningEffort ?? (activeModelName ? selectedCapability?.default_effort : undefined) ?? settings?.reasoning_effort ?? '') || ''

  const EFFORT_LABELS: Record<string, string> = {
    '': '默认',
    none: '关闭',
    low: '轻量',
    medium: '中等',
    high: '深度',
    max: '最大',
  }

  const formatEffort = (effort: string): string => {
    if (!effort) return '默认'
    const name = EFFORT_LABELS[effort]
    return name ? `${name} (${effort})` : effort
  }

  const effortDisplay = formatEffort(currentEffort)
  const triggerEffortLabel = currentEffort ? (EFFORT_LABELS[currentEffort] ?? currentEffort) : undefined

  const effortOptions = useMemo<{ key: string; label: string }[]>(() => {
    if (supportedEfforts.length > 0) {
      const opts = [{ key: '', label: '默认 (模型默认)' }]
      for (const eff of supportedEfforts) {
        opts.push({ key: eff, label: formatEffort(eff) })
      }
      return opts
    }
    return [
      { key: '', label: '默认 (模型默认)' },
      { key: 'none', label: '关闭 (none)' },
      { key: 'low', label: '轻量 (low)' },
      { key: 'medium', label: '中等 (medium)' },
      { key: 'high', label: '深度 (high)' },
      { key: 'max', label: '最大 (max)' },
    ]
  }, [supportedEfforts])

  return (
    <div className={clsx(codexCss.root, local.anchor, variant === 'hero' && codexCss.hero)}>
      {note !== undefined && (
        <div
          className={codexCss.notice}
          role="status"
          style={{ cursor: onOpenSettings ? 'pointer' : undefined }}
          onClick={() => { onOpenSettings?.('models') }}
        >
          {note}
        </div>
      )}
      {/* DSH Full Access Risk Notice Banner */}
      {!bannerDismissed && permissionMode === 'full_access' && (
        <div className={codexCss.permissionBanner}>
          <div className={codexCss.bannerLeft}>
            <span className={codexCss.infoIcon}>ⓘ</span>
            <div className={codexCss.bannerContent}>
              <span className={codexCss.bannerTitle}>完全权限已开启</span>
              <span className={codexCss.bannerText}>
                启用完全权限后，智能体可修改工作区文件，并直接运行外部命令。命令不受文件工具的工作区路径限制，仅建议在你信任当前任务时使用。{' '}
                <a
                  href="#learn-more"
                  className={codexCss.learnMoreLink}
                  onClick={(e) => {
                    e.preventDefault()
                    onOpenSettings?.('general')
                  }}
                >
                  了解更多关于安全权限的信息。
                </a>
              </span>
            </div>
          </div>
          <div className={codexCss.bannerActions}>
            <button
              type="button"
              className={codexCss.dismissBtn}
              onClick={() => {
                setBannerDismissed(true)
                try {
                  localStorage.setItem('codex_perm_banner_dismissed', 'true')
                } catch {}
              }}
            >
              不再显示
            </button>
            <button
              type="button"
              className={codexCss.closeBannerBtn}
              title="关闭提示"
              aria-label="关闭提示"
              onClick={() => setBannerDismissed(true)}
            >
              ✕
            </button>
          </div>
        </div>
      )}

      {/* Outer Composer Container */}
      <div className={codexCss.composerBox}>
        {/* DSH Workspace Row (Sits 12px above the card in hero mode) */}
        {variant === 'hero' && (
          <div className={heroShellCss.workspaceRow} style={{ marginBottom: 12, paddingLeft: 8 }}>
            <button
              ref={workspaceAnchorRef}
              type="button"
              className={heroShellCss.workspace}
              aria-label="选择工作区"
              aria-haspopup="menu"
              aria-expanded={workspaceMenuOpen}
              onClick={() => setWorkspaceMenuOpen(open => !open)}
            >
              <IconFolderOpenRegular className={heroShellCss.folder} size={16} />
              <span className={heroShellCss.workspaceLabel}>{wsName}</span>
              <IconChevronDownOutlineRegular className={heroShellCss.chevron} size={12} />
            </button>

            <Menu
              open={workspaceMenuOpen}
              onClose={() => setWorkspaceMenuOpen(false)}
              anchor={null}
              portal
              getAnchorRect={() => workspaceAnchorRef.current?.getBoundingClientRect() ?? null}
              items={(workspaces.length > 0 ? workspaces : [activeWorkspace || workspacePath || 'codex-workspace-mcp']).map(ws => ({
                id: ws,
                label: workspaceName(ws),
                icon: <IconFolderCloseRegular size={16} />,
              }))}
              selectedId={activeWorkspace || workspacePath}
              onSelect={(id) => {
                if (id === '::add_workspace') {
                  setWorkspaceMenuOpen(false)
                  onOpenAddWorkspace?.()
                } else {
                  setWorkspaceMenuOpen(false)
                  onSelectWorkspace?.(id)
                }
              }}
              footer={[
                {
                  id: '::add_workspace',
                  label: '选择文件夹 / 添加工作区...',
                  icon: <IconPlusOutlineRegular size={16} />,
                },
              ]}
            />
          </div>
        )}

        {/* Layer 2: White Floating Input Card */}
        <div className={codexCss.card} data-composer-card>
          <div className={codexCss.inputWrap}>
            <textarea
              ref={inputRef}
              className={codexCss.textarea}
              rows={1}
              value={draft}
              disabled={disabled}
              placeholder={placeholder}
              aria-label={placeholder}
              onChange={(event) => {
                setDraft(event.target.value)
                setCaret(event.target.selectionStart)
                setMenuIndex(0)
              }}
              onSelect={syncCaret}
              onKeyUp={syncCaret}
              onClick={syncCaret}
              onKeyDown={onKeyDown}
            />
          </div>

          <div className={codexCss.toolbar}>
            <div className={codexCss.leftControls}>
              {/* Bare Plus Button matching media_1791084397752.png */}
              <Tooltip label="添加上下文或文件 (@)" side="top" delayMs={500}>
                <button
                  type="button"
                  className={codexCss.barePlusBtn}
                  title="添加上下文"
                  onClick={() => {
                    const next = draft ? `${draft} @` : '@'
                    setDraft(next)
                    setCaret(next.length)
                    setMenuIndex(0)
                    window.requestAnimationFrame(() => {
                      inputRef.current?.focus()
                      inputRef.current?.setSelectionRange(next.length, next.length)
                    })
                  }}
                >
                  <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.2" strokeLinecap="round">
                    <line x1="12" y1="5" x2="12" y2="19" />
                    <line x1="5" y1="12" x2="19" y2="12" />
                  </svg>
                </button>
              </Tooltip>

              {/* DSH Permission Badge with Codex styling (media_1791084397752.png & media_1791085181305.png) */}
              <button
                ref={permTriggerRef}
                type="button"
                className={clsx(
                  codexCss.permPill,
                  permissionMode === 'workspace_write' && codexCss.permPillNeutral,
                  permissionMode === 'read_only' && codexCss.permPillSafe,
                )}
                onClick={togglePermDropdown}
                title="切换工作区操作权限"
              >
                {permissionMode === 'full_access' && (
                  <>
                    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8">
                      <circle cx="8" cy="8" r="7" />
                      <line x1="8" y1="4.5" x2="8" y2="8.5" />
                      <circle cx="8" cy="11.5" r="0.8" fill="currentColor" />
                    </svg>
                    <span>完全权限</span>
                  </>
                )}
                {permissionMode === 'workspace_write' && (
                  <>
                    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" strokeWidth="1.3">
                      <path d="M6.4209 1.68067C7.43922 1.299 8.56177 1.29898 9.58008 1.68067L14.0996 3.375C14.2946 3.44811 14.4236 3.63455 14.4238 3.84278V6.89063C14.1115 6.71853 13.7761 6.58312 13.4238 6.48926V4.18946L9.22852 2.61621C8.43657 2.31947 7.56341 2.31939 6.77148 2.61621L2.5752 4.18946V7.11914C2.5752 11.1796 5.52369 13.056 8 14.0391C8.27653 13.9293 8.55827 13.8067 8.8418 13.6729C9.07101 13.9468 9.33228 14.1929 9.62012 14.4053C9.12409 14.6579 8.63578 14.8696 8.17871 15.0449C8.0637 15.0889 7.93628 15.0889 7.82129 15.0449C5.22011 14.0472 1.5752 11.9381 1.5752 7.11914V3.84278C1.57541 3.63469 1.70463 3.44821 1.89941 3.375L6.4209 1.68067Z" fill="currentColor" />
                      <path d="M5.26392 6.60339H10.7361" stroke="currentColor" strokeLinecap="round" />
                      <path d="M5.26392 9.86902H8.32833" stroke="currentColor" strokeLinecap="round" />
                    </svg>
                    <span>工作区内修改</span>
                  </>
                )}
                {permissionMode === 'read_only' && (
                  <>
                    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" strokeWidth="1.3" stroke="currentColor">
                      <path d="M5.08545 8.13775L7.18455 10.2368C7.26636 10.3187 7.4003 10.3142 7.47649 10.2271L11.5148 5.61194" strokeLinecap="round" strokeLinejoin="round" />
                      <path d="M6.59624 2.14853C7.50155 1.80917 8.49914 1.80919 9.40444 2.14859L13.9245 3.84317V7.11961C13.9245 11.6089 10.5565 13.5975 8.00035 14.5779C5.44423 13.5975 2.07544 11.6089 2.07544 7.11961V3.84317L6.59624 2.14853Z" strokeLinejoin="round" />
                    </svg>
                    <span>仅可查看</span>
                  </>
                )}
              </button>

              {subagentEnabled && (
                <Tooltip label="子代理委派已激活" side="top" delayMs={500}>
                  <button
                    type="button"
                    className={codexCss.permPill}
                    style={{ color: '#2563eb' }}
                    onClick={() => { if (onToggleSubagent !== undefined) void onToggleSubagent() }}
                  >
                    <span>子代理</span>
                  </button>
                </Tooltip>
              )}
            </div>

            <div className={codexCss.rightControls}>
              {/* Model Selector Pill */}
              <button
                ref={modelTriggerRef}
                type="button"
                className={codexCss.modelPill}
                aria-haspopup="menu"
                aria-expanded={modelMenuOpen}
                title={triggerEffortLabel ? `${displayModel} · ${triggerEffortLabel}` : displayModel}
                onClick={openModelDropdown}
              >
                <span>{displayModel}</span>
                {triggerEffortLabel !== undefined && (
                  <span className={codexCss.effortTag}>{triggerEffortLabel}</span>
                )}
                {fastMode && <span>⚡</span>}
                <IconChevronDownOutlineRegular className={codexCss.chevronDown} size={11} />
              </button>

              {/* Bare Mic Button */}
              <button
                type="button"
                className={codexCss.micBtn}
                title="语音输入"
                onClick={() => {}}
              >
                <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" />
                  <path d="M19 10v2a7 7 0 0 1-14 0v-2" />
                  <line x1="12" y1="19" x2="12" y2="22" />
                </svg>
              </button>

              {/* Blue Action Circle: Stop / Send / Audio Wave (media_1791084397752.png) */}
              {running && onStop !== undefined ? (
                <Tooltip label={t('input.stop')} side="top" delayMs={500}>
                  <button
                    type="button"
                    className={codexCss.stopBtn}
                    aria-label={t('input.stop')}
                    onClick={onStop}
                  >
                    {STOP_ICON}
                  </button>
                </Tooltip>
              ) : draft.trim() !== '' ? (
                <Tooltip label={t('input.send')} side="top" delayMs={500} disabled={!canSend}>
                  <button
                    type="button"
                    className={codexCss.actionBtn}
                    aria-label={t('input.send')}
                    disabled={!canSend}
                    onClick={() => { void submit() }}
                  >
                    <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.6" strokeLinecap="round" strokeLinejoin="round">
                      <line x1="12" y1="19" x2="12" y2="5" />
                      <polyline points="5 12 12 5 19 12" />
                    </svg>
                  </button>
                </Tooltip>
              ) : (
                <Tooltip label="实时语音 (Voice Mode)" side="top" delayMs={500}>
                  <button
                    type="button"
                    className={codexCss.actionBtn}
                    aria-label="实时语音"
                    onClick={() => { inputRef.current?.focus() }}
                  >
                    <svg width="15" height="15" viewBox="0 0 17 16" fill="currentColor">
                      <rect x="2.5" y="5.5" width="1.8" height="5" rx="0.9" />
                      <rect x="6" y="2" width="1.8" height="12" rx="0.9" />
                      <rect x="9.5" y="4" width="1.8" height="8" rx="0.9" />
                      <rect x="13" y="6" width="1.8" height="4" rx="0.9" />
                    </svg>
                  </button>
                </Tooltip>
              )}
            </div>
          </div>
        </div>
      </div>

      {menuOpen && (
        <div className={local.suggest} role="listbox" aria-label={token?.kind === '@' ? '引用文件' : '技能指令'}>
          {suggestions.map((suggestion, index) => (
            <button
              key={suggestion.id}
              type="button"
              role="option"
              aria-selected={index === activeIndex}
              className={clsx(local.suggestOption, index === activeIndex && local.suggestActive)}
              onMouseDown={(event) => {
                event.preventDefault()
                applySuggestion(suggestion)
              }}
            >
              <span className={local.suggestLabel}>{suggestion.label}</span>
              {suggestion.detail !== '' && <span className={local.suggestDetail}>{suggestion.detail}</span>}
            </button>
          ))}
        </div>
      )}

      {modelMenuOpen && createPortal(
        <div
          ref={modelMenuRef}
          className={modelCss.menu}
          style={{
            top: menuPos.top,
            bottom: menuPos.bottom,
            right: menuPos.right,
          }}
          role="menu"
          aria-label="模型选择与推理设置"
        >
          {pane === 'root' && (
            <>
              <button
                type="button"
                role="menuitem"
                className={modelCss.cell}
                onClick={() => { setPane('model') }}
              >
                <span className={modelCss.cellLabel}>模型</span>
                <span className={modelCss.cellValue}>{activeModelName || '未选择'}</span>
                <IconChevronRightOutlineRegular className={modelCss.cellChevron} size={12} />
              </button>
              <button
                type="button"
                role="menuitem"
                className={modelCss.cell}
                onClick={() => { setPane('effort') }}
              >
                <span className={modelCss.cellLabel}>思考等级</span>
                <span className={modelCss.cellValue}>{effortDisplay}</span>
                <IconChevronRightOutlineRegular className={modelCss.cellChevron} size={12} />
              </button>
              <div
                className={modelCss.cell}
                style={{ justifyContent: 'space-between', cursor: 'default' }}
              >
                <span className={modelCss.cellLabel}>⚡ 请求加速</span>
                <Switch
                  checked={Boolean(fastMode)}
                  label="请求加速"
                  disabled={onSetGenerationOptions === undefined}
                  onChange={(checked) => {
                    void onSetGenerationOptions?.(currentEffort, checked)
                  }}
                />
              </div>
              <div style={{ borderTop: '0.5px solid var(--dsw-alias-border-l2)', margin: '3px 0' }} />
              <button
                type="button"
                role="menuitem"
                className={modelCss.cell}
                onClick={() => {
                  setModelMenuOpen(false)
                  onOpenSettings?.('models')
                }}
              >
                <span className={modelCss.cellLabel}>配置模型提供商…</span>
                <span className={modelCss.cellValue}>{providers.length} 个提供商</span>
                <IconChevronRightOutlineRegular className={modelCss.cellChevron} size={12} />
              </button>
            </>
          )}

          {pane === 'model' && (
            <>
              <button
                type="button"
                role="menuitem"
                className={modelCss.cell}
                style={{ marginBottom: 3 }}
                onClick={() => { setPane('root') }}
              >
                <IconChevronLeftOutlineRegular className={modelCss.cellChevron} size={12} />
                <span className={modelCss.cellLabel} style={{ fontWeight: 500 }}>所有模型</span>
              </button>
              <div style={{ borderTop: '0.5px solid var(--dsw-alias-border-l2)', marginBottom: 3 }} />
              <div className={clsx(modelCss.groups, 'scrollable')}>
                {providers.length === 0 ? (
                  <div className={modelCss.empty}>尚未配置模型提供商</div>
                ) : (
                  providers.map(provider => (
                    <section key={provider.id} className={modelCss.group}>
                      <div className={modelCss.groupTitle}>{provider.display_name || provider.id}</div>
                      {provider.models.map((m) => {
                        const selected = m === activeModelName
                        return (
                          <button
                            key={`${provider.id}:${m}`}
                            type="button"
                            role="menuitemradio"
                            aria-checked={selected}
                            className={clsx(modelCss.option, selected && modelCss.selected)}
                            onClick={() => {
                              setModelMenuOpen(false)
                              void onSelectModel?.(provider.id, m)
                            }}
                          >
                            <span className={modelCss.optionCopy}>
                              <span className={modelCss.modelName}>{m}</span>
                            </span>
                            <span className={modelCss.check}>
                              {selected ? <IconCheckOutlineRegular size={14} /> : null}
                            </span>
                          </button>
                        )
                      })}
                    </section>
                  ))
                )}
              </div>
            </>
          )}

          {pane === 'effort' && (
            <>
              <button
                type="button"
                role="menuitem"
                className={modelCss.cell}
                style={{ marginBottom: 3 }}
                onClick={() => { setPane('root') }}
              >
                <IconChevronLeftOutlineRegular className={modelCss.cellChevron} size={12} />
                <span className={modelCss.cellLabel} style={{ fontWeight: 500 }}>思考等级</span>
              </button>
              <div style={{ borderTop: '0.5px solid var(--dsw-alias-border-l2)', marginBottom: 3 }} />
              <div className={clsx(modelCss.groups, 'scrollable')}>
                {effortOptions.map((opt) => {
                  const selected = currentEffort === opt.key
                  return (
                    <button
                      key={opt.key || 'default'}
                      type="button"
                      role="menuitemradio"
                      aria-checked={selected}
                      className={clsx(modelCss.option, selected && modelCss.selected)}
                      onClick={() => {
                        void onSetGenerationOptions?.(opt.key, Boolean(fastMode))
                        setPane('root')
                      }}
                    >
                      <span className={modelCss.optionCopy}>
                        <span className={modelCss.modelName}>{opt.label}</span>
                      </span>
                      <span className={modelCss.check}>
                        {selected ? <IconCheckOutlineRegular size={14} /> : null}
                      </span>
                    </button>
                  )
                })}
              </div>
            </>
          )}
        </div>,
        document.body,
      )}

      {/* Codex-styled Permission Popover for DSH Permissions (media_1791085181305.png) */}
      {permMenuOpen && createPortal(
        <div
          ref={permMenuRef}
          className={codexCss.permPopover}
          style={{
            top: permMenuPos.top,
            bottom: permMenuPos.bottom,
            left: permMenuPos.left,
          }}
          role="dialog"
          aria-label="工作区操作权限"
        >
          <div className={codexCss.permPopoverHeader}>
            <span className={codexCss.permPopoverTitle}>工作区操作权限</span>
            <button
              type="button"
              className={codexCss.permPopoverLearnMore}
              onClick={() => {
                setPermMenuOpen(false)
                onOpenSettings?.('general')
              }}
            >
              了解更多
            </button>
          </div>

          <div className={codexCss.permOptionList}>
            {/* 1. 仅可查看 (Read Only) */}
            <button
              type="button"
              className={codexCss.permOptionItem}
              onClick={() => handleSelectPermission('read_only')}
            >
              <span className={codexCss.permOptionIcon}>
                <svg width="18" height="18" viewBox="0 0 16 16" fill="none" strokeWidth="1.3" stroke="currentColor">
                  <path d="M5.08545 8.13775L7.18455 10.2368C7.26636 10.3187 7.4003 10.3142 7.47649 10.2271L11.5148 5.61194" strokeLinecap="round" strokeLinejoin="round" />
                  <path d="M6.59624 2.14853C7.50155 1.80917 8.49914 1.80919 9.40444 2.14859L13.9245 3.84317V7.11961C13.9245 11.6089 10.5565 13.5975 8.00035 14.5779C5.44423 13.5975 2.07544 11.6089 2.07544 7.11961V3.84317L6.59624 2.14853Z" strokeLinejoin="round" />
                </svg>
              </span>
              <div className={codexCss.permOptionBody}>
                <span className={codexCss.permOptionTitle}>仅可查看</span>
                <span className={codexCss.permOptionDesc}>只读模式，禁止修改文件或运行外部命令</span>
              </div>
              {permissionMode === 'read_only' && (
                <span className={codexCss.permOptionCheck}>
                  <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="20 6 9 17 4 12" />
                  </svg>
                </span>
              )}
            </button>

            {/* 2. 工作区内修改 (Workspace Write) */}
            <button
              type="button"
              className={codexCss.permOptionItem}
              onClick={() => handleSelectPermission('workspace_write')}
            >
              <span className={codexCss.permOptionIcon}>
                <svg width="18" height="18" viewBox="0 0 16 16" fill="none" strokeWidth="1.3">
                  <path d="M6.4209 1.68067C7.43922 1.299 8.56177 1.29898 9.58008 1.68067L14.0996 3.375C14.2946 3.44811 14.4236 3.63455 14.4238 3.84278V6.89063C14.1115 6.71853 13.7761 6.58312 13.4238 6.48926V4.18946L9.22852 2.61621C8.43657 2.31947 7.56341 2.31939 6.77148 2.61621L2.5752 4.18946V7.11914C2.5752 11.1796 5.52369 13.056 8 14.0391C8.27653 13.9293 8.55827 13.8067 8.8418 13.6729C9.07101 13.9468 9.33228 14.1929 9.62012 14.4053C9.12409 14.6579 8.63578 14.8696 8.17871 15.0449C8.0637 15.0889 7.93628 15.0889 7.82129 15.0449C5.22011 14.0472 1.5752 11.9381 1.5752 7.11914V3.84278C1.57541 3.63469 1.70463 3.44821 1.89941 3.375L6.4209 1.68067Z" fill="currentColor" />
                  <path d="M5.26392 6.60339H10.7361" stroke="currentColor" strokeLinecap="round" />
                  <path d="M5.26392 9.86902H8.32833" stroke="currentColor" strokeLinecap="round" />
                </svg>
              </span>
              <div className={codexCss.permOptionBody}>
                <span className={codexCss.permOptionTitle}>工作区内修改</span>
                <span className={codexCss.permOptionDesc}>允许修改工作区文件；不开放未隔离的外部命令</span>
              </div>
              {permissionMode === 'workspace_write' && (
                <span className={codexCss.permOptionCheck}>
                  <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="20 6 9 17 4 12" />
                  </svg>
                </span>
              )}
            </button>

            {/* 3. 完全权限 (Full Access) */}
            <button
              type="button"
              className={codexCss.permOptionItem}
              onClick={() => handleSelectPermission('full_access')}
            >
              <span className={clsx(codexCss.permOptionIcon, codexCss.permOptionIconWarn)}>
                <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.8">
                  <circle cx="8" cy="8" r="7" />
                  <line x1="8" y1="4.5" x2="8" y2="8.5" />
                  <circle cx="8" cy="11.5" r="0.8" fill="currentColor" />
                </svg>
              </span>
              <div className={codexCss.permOptionBody}>
                <span className={clsx(codexCss.permOptionTitle, codexCss.permOptionTitleWarn)}>完全权限</span>
                <span className={codexCss.permOptionDesc}>允许修改工作区文件及执行外部命令；文件工具仍检查工作区路径</span>
              </div>
              {permissionMode === 'full_access' && (
                <span className={codexCss.permOptionCheck}>
                  <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="20 6 9 17 4 12" />
                  </svg>
                </span>
              )}
            </button>
          </div>
        </div>,
        document.body,
      )}

      {/* DSH Native Risk Confirmation Dialog */}
      {showRiskConfirm && (
        <RiskConfirmation
          open={showRiskConfirm}
          title="确认启用完全权限？"
          description="启用完全权限后，智能体可修改工作区文件，并直接运行外部命令。命令不受文件工具的工作区路径限制，仅建议在你信任当前任务时使用。"
          acknowledgeLabel="我已了解风险，并愿意继续"
          cancelLabel="取消"
          closeLabel="关闭"
          confirmLabel="启用完全权限"
          acknowledged={riskAcknowledged}
          onAcknowledgedChange={setRiskAcknowledged}
          onCancel={() => {
            setShowRiskConfirm(false)
            setRiskAcknowledged(false)
          }}
          onConfirm={() => {
            setShowRiskConfirm(false)
            setRiskAcknowledged(false)
            selectPermissionMode('full_access')
          }}
        />
      )}
    </div>
  )
}
