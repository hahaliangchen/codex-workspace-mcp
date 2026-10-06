import { useCallback, useEffect, useMemo, useState } from 'react'
import { createPortal } from 'react-dom'
import clsx from 'clsx'
import {
  IconChevronDownOutlineRegular,
  IconCloseOutlineRegular,
  IconCodeOutlineRegular,
  IconCordisPluginOutlineRegular,
  IconDarkOutlineRegular,
  IconDatabaseOutlineRegular,
  IconLightOutlineRegular,
  IconPlusOutlineRegular,
  IconSearchOutlineRegular,
  IconSettingsOutlineRegular,
  IconUsersOutlineRegular,
  StateDot,
  Switch,
} from '@deepseek-ai/dsh-client-ui-primitives'
import {
  api,
  REASONING_LEVELS,
  type ModelCapabilities,
  type PluginsSnapshot,
  type ProviderApiType,
  type SettingsData,
} from '../api.ts'
import appearanceCss from '../dsh/settings/AppearanceRow.module.css'
import modelsCss from '../dsh/settings/ModelsSection.module.css'
import pluginsCss from '../dsh/settings/PluginInventorySettingsTab.module.css'
import rootCss from '../dsh/settings/SettingsRoot.module.css'
import subagentCss from '../dsh/settings/SubagentCard.module.css'
import { getThemeMode, setThemeMode, type ThemeMode } from '../theme.ts'
import type { SettingsTab } from '../useAgentWorkspace.ts'

interface DraftProvider {
  _draftId: string
  _new: boolean
  id: string
  display_name: string
  url: string
  api_type: ProviderApiType
  models_text: string
  model_capabilities: Record<string, ModelCapabilities>
  has_api_key: boolean
  api_key_env: string | null
  api_key: string
  clear_api_key: boolean
}

interface DraftSettings {
  visual_fallback_enabled: boolean
  visual_provider: string
  visual_model: string
  revision: string
  providers: DraftProvider[]
  orchestrator_provider: string
  orchestrator_model: string
  reasoning_effort: string
  fast_mode: boolean
  observer_enabled: boolean
  observer_provider: string
  observer_model: string
  expert_provider: string
  expert_model: string
  enable_subagent: boolean
}

function parseModels(lines: string): string[] {
  const models: string[] = []
  const seen = new Set<string>()
  for (const raw of lines.split(/\r?\n/)) {
    const line = raw.trim()
    if (!line) continue
    if (!seen.has(line)) {
      seen.add(line)
      models.push(line)
    }
  }
  return models
}

function toDraft(data: SettingsData): DraftSettings {
  return {
    revision: data.revision,
    providers: data.providers.map((p, index) => ({
      _draftId: `saved-${index}-${p.id}`,
      _new: false,
      id: p.id,
      display_name: p.display_name ?? '',
      url: p.url,
      api_type: p.api_type,
      models_text: (p.models ?? []).join('\n'),
      model_capabilities: { ...p.model_capabilities },
      has_api_key: Boolean(p.has_api_key),
      api_key_env: p.api_key_env ?? null,
      api_key: '',
      clear_api_key: false,
    })),
    orchestrator_provider: data.orchestrator_provider ?? '',
    orchestrator_model: data.orchestrator_model ?? '',
    reasoning_effort: data.reasoning_effort ?? '',
    fast_mode: data.fast_mode,
    observer_enabled: Boolean(data.observer_enabled),
    observer_provider: data.observer_provider ?? '',
    observer_model: data.observer_model ?? '',
    expert_provider: data.expert_provider ?? '',
    expert_model: data.expert_model ?? '',
    enable_subagent: Boolean(data.enable_subagent),
    visual_fallback_enabled: Boolean(data.visual_fallback_enabled),
    visual_provider: data.visual_provider ?? '',
    visual_model: data.visual_model ?? '',
  }
}

function protocolTag(apiType: ProviderApiType): string {
  switch (apiType) {
    case 'anthropic-messages': return 'Anthropic'
    case 'openai-responses': return 'Responses'
    default: return 'OpenAI'
  }
}

export function SettingsModal({
  open,
  activeTab,
  workspacePath,
  onSelectTab,
  onClose,
  onThemeChanged,
  onSaved,
}: {
  open: boolean
  activeTab: SettingsTab
  workspacePath: string
  onSelectTab: (tab: SettingsTab) => void
  onClose: () => void
  onThemeChanged: () => void
  onSaved: () => void
}) {
  const [themeMode, setThemeModeState] = useState<ThemeMode>(getThemeMode)
  const [draft, setDraft] = useState<DraftSettings | null>(null)
  const [openProviderId, setOpenProviderId] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [statusMsg, setStatusMsg] = useState<{ text: string; error: boolean } | null>(null)
  const [discoveringId, setDiscoveringId] = useState<string | null>(null)
  const [discoveredModels, setDiscoveredModels] = useState<Record<string, string[]>>({})
  const [discoverFilter, setDiscoverFilter] = useState('')
  const [pluginsData, setPluginsData] = useState<PluginsSnapshot | null>(null)
  const [pluginQuery, setPluginQuery] = useState('')
  const [expandedPlugins, setExpandedPlugins] = useState<Record<string, boolean>>({})

  const loadSettings = useCallback(() => {
    setStatusMsg(null)
    api.getSettings().then(
      (data) => {
        const next = toDraft(data)
        setDraft(next)
        if (next.providers.length === 1 && openProviderId === null) {
          setOpenProviderId(next.providers[0]?._draftId ?? null)
        }
      },
      (err: unknown) => {
        setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
      },
    )
  }, [openProviderId])

  const loadPlugins = useCallback(() => {
    api.getPlugins().then(
      data => { setPluginsData(data) },
      (err: unknown) => {
        setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
      },
    )
  }, [])

  useEffect(() => {
    if (!open) return
    setThemeModeState(getThemeMode())
    loadSettings()
    loadPlugins()
  }, [open, loadSettings, loadPlugins])

  useEffect(() => {
    if (!open) return
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => { window.removeEventListener('keydown', onKeyDown) }
  }, [open, onClose])

  const updateDraft = (patch: Partial<DraftSettings>) => {
    setDraft(prev => (prev === null ? null : { ...prev, ...patch }))
    setStatusMsg(null)
  }

  const updateProvider = (draftId: string, patch: Partial<DraftProvider>) => {
    setDraft(prev => {
      if (prev === null) return null
      return {
        ...prev,
        providers: prev.providers.map(p => (p._draftId === draftId ? { ...p, ...patch } : p)),
      }
    })
    setStatusMsg(null)
  }

  const updateModelCapability = (provider: DraftProvider, model: string, patch: Partial<ModelCapabilities>) => {
    const current = provider.model_capabilities[model] ?? { reasoning_efforts: [], default_effort: null, fast_mode: false }
    updateProvider(provider._draftId, {
      model_capabilities: {
        ...provider.model_capabilities,
        [model]: { ...current, ...patch },
      },
    })
  }

  const handleSave = async () => {
    if (draft === null || saving) return
    setSaving(true)
    setStatusMsg(null)
    try {
      const providersPayload = draft.providers.map((p) => {
        const id = p.id.trim()
        if (!id) throw new Error('Provider ID 不能为空')
        const models = parseModels(p.models_text)
        if (models.length === 0) throw new Error(`提供商「${p.display_name || id}」至少需要配置一个模型 ID`)
        return {
          id,
          display_name: p.display_name.trim() || null,
          url: p.url.trim(),
          api_type: p.api_type,
          models,
          model_capabilities: Object.fromEntries(models.flatMap(model => {
            const capability = p.model_capabilities[model]
            return capability === undefined ? [] : [[model, capability] as const]
          })),
          ...(p.api_key.trim() !== '' ? { api_key: p.api_key.trim() } : {}),
          clear_api_key: p.clear_api_key,
        }
      })
      const selectedCapability = draft.providers.find(p => p.id === draft.orchestrator_provider)
        ?.model_capabilities[draft.orchestrator_model]
      const saved = await api.saveSettings({
        revision: draft.revision,
        providers: providersPayload,
        orchestrator_provider: draft.orchestrator_provider || null,
        orchestrator_model: draft.orchestrator_model || null,
        reasoning_effort: selectedCapability?.reasoning_efforts.includes(draft.reasoning_effort) ? draft.reasoning_effort : '',
        fast_mode: Boolean(selectedCapability?.fast_mode && draft.fast_mode),
        expert_provider: draft.expert_provider || null,
        expert_model: draft.expert_model || null,
        enable_subagent: draft.enable_subagent,
        observer_enabled: draft.observer_enabled,
        observer_provider: draft.observer_provider || null,
        observer_model: draft.observer_model || null,
        visual_fallback_enabled: draft.visual_fallback_enabled,
        visual_provider: draft.visual_provider || null,
        visual_model: draft.visual_model || null,
      })
      setDraft(toDraft(saved))
      setStatusMsg({ text: '设置已保存并立即对新任务生效。', error: false })
      onSaved()
    } catch (err) {
      setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
    } finally {
      setSaving(false)
    }
  }

  const handleAddProvider = () => {
    if (draft === null) return
    let num = 1
    while (draft.providers.some(p => p.id === `provider-${num}`)) num += 1
    const created: DraftProvider = {
      _draftId: `new-${Date.now()}-${num}`,
      _new: true,
      id: `provider-${num}`,
      display_name: '',
      url: 'https://api.deepseek.com/v1',
      api_type: 'openai-completions',
      models_text: 'deepseek-chat\ndeepseek-reasoner',
      model_capabilities: {},
      has_api_key: false,
      api_key_env: null,
      api_key: '',
      clear_api_key: false,
    }
    const nextProviders = [...draft.providers, created]
    updateDraft({
      providers: nextProviders,
      orchestrator_provider: draft.orchestrator_provider || created.id,
      orchestrator_model: draft.orchestrator_model || 'deepseek-chat',
    })
    setOpenProviderId(created._draftId)
  }

  const handleDeleteProvider = (provider: DraftProvider) => {
    if (draft === null) return
    const nextProviders = draft.providers.filter(p => p._draftId !== provider._draftId)
    updateDraft({
      providers: nextProviders,
      orchestrator_provider: draft.orchestrator_provider === provider.id ? (nextProviders[0]?.id ?? '') : draft.orchestrator_provider,
      orchestrator_model: draft.orchestrator_provider === provider.id ? '' : draft.orchestrator_model,
      expert_provider: draft.expert_provider === provider.id ? '' : draft.expert_provider,
      expert_model: draft.expert_provider === provider.id ? '' : draft.expert_model,
      observer_provider: draft.observer_provider === provider.id ? '' : draft.observer_provider,
      observer_model: draft.observer_provider === provider.id ? '' : draft.observer_model,
    })
    if (openProviderId === provider._draftId) setOpenProviderId(null)
  }

  const handleDiscover = async (provider: DraftProvider) => {
    setDiscoveringId(provider._draftId)
    setDiscoverFilter('')
    try {
      const res = await api.discoverModels({
        id: provider.id,
        url: provider.url,
        ...(provider.api_key ? { api_key: provider.api_key } : {}),
      })
      setDiscoveredModels(prev => ({ ...prev, [provider._draftId]: res.models ?? [] }))
    } catch (err) {
      setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
    } finally {
      setDiscoveringId(null)
    }
  }

  const toggleCandidateModel = (provider: DraftProvider, modelId: string) => {
    const current = parseModels(provider.models_text)
    const exists = current.includes(modelId)
    const next = exists ? current.filter(m => m !== modelId) : [...current, modelId]
    updateProvider(provider._draftId, { models_text: next.join('\n') })
  }

  const mainProviderObj = useMemo(
    () => draft?.providers.find(p => p.id === draft.orchestrator_provider),
    [draft],
  )
  const mainModels = useMemo(
    () => (mainProviderObj ? parseModels(mainProviderObj.models_text) : []),
    [mainProviderObj],
  )
  const mainCapability = mainProviderObj?.model_capabilities[draft?.orchestrator_model ?? '']

  const expertProviderObj = useMemo(
    () => draft?.providers.find(p => p.id === draft.expert_provider),
    [draft],
  )
  const expertModels = useMemo(
    () => (expertProviderObj ? parseModels(expertProviderObj.models_text) : []),
    [expertProviderObj],
  )

  const observerProviderObj = useMemo(
    () => draft?.providers.find(p => p.id === draft.observer_provider),
    [draft],
  )
  const observerModels = useMemo(
    () => (observerProviderObj ? parseModels(observerProviderObj.models_text) : []),
    [observerProviderObj],
  )

  if (!open) return null

  const navItems: readonly { id: SettingsTab; label: string; icon: JSX.Element }[] = [
    { id: 'general', label: '通用', icon: <IconSettingsOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'models', label: '模型与提供商', icon: <IconDatabaseOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'observer', label: 'Observer', icon: <IconSearchOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'subagent', label: '子代理', icon: <IconUsersOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'plugins', label: '插件状态', icon: <IconCordisPluginOutlineRegular className={rootCss.navIcon} size={18} /> },
  ]

  return createPortal(
    <div className={rootCss.overlay} role="dialog" aria-modal="true" aria-label="设置">
      <div className={rootCss.mask} onClick={onClose} />
      <div className={rootCss.panel}>
        <nav className={rootCss.nav} aria-label="设置导航">
          <div className={rootCss.navTitle}>设置</div>
          <div className={rootCss.navList}>
            {navItems.map(item => (
              <button
                key={item.id}
                type="button"
                className={clsx(rootCss.navCell, activeTab === item.id && rootCss.active)}
                onClick={() => { onSelectTab(item.id) }}
              >
                {item.icon}
                <span className={rootCss.navLabel}>{item.label}</span>
              </button>
            ))}
          </div>
        </nav>

        <div className={rootCss.content}>
          <header className={rootCss.header}>
            <div className={rootCss.actions}>
              {activeTab !== 'plugins' && (
                <button
                  type="button"
                  className={modelsCss.primaryButton}
                  disabled={saving || draft === null}
                  onClick={() => { void handleSave() }}
                >
                  {saving ? '保存中…' : '保存更改'}
                </button>
              )}
              <button type="button" className={rootCss.close} aria-label="关闭设置" onClick={onClose}>
                <IconCloseOutlineRegular size={16} />
              </button>
            </div>
          </header>

          <div className={rootCss.options}>
            {statusMsg !== null && (
              <p className={statusMsg.error ? modelsCss.error : modelsCss.savedNotice} style={{ marginBottom: 12 }}>
                {statusMsg.text}
              </p>
            )}

            {activeTab === 'general' && (
              <div className={modelsCss.section}>
                <h2 className={modelsCss.title}>通用设置</h2>
                <p className={modelsCss.intro}>自定义 DeepSeek Harness 风格界面的外观主题与工作区环境。</p>

                <div className={appearanceCss.group}>
                  <div className={appearanceCss.title}>外观</div>
                  <div className={appearanceCss.cubeRow}>
                    {([
                      { mode: 'light', label: '浅色', icon: <IconLightOutlineRegular size={22} /> },
                      { mode: 'dark', label: '深色', icon: <IconDarkOutlineRegular size={22} /> },
                      { mode: 'system', label: '跟随系统', icon: <IconCodeOutlineRegular size={22} /> },
                    ] as const).map(option => (
                      <button
                        key={option.mode}
                        type="button"
                        className={clsx(appearanceCss.themeCube, themeMode === option.mode && appearanceCss.selected)}
                        onClick={() => {
                          setThemeModeState(option.mode)
                          setThemeMode(option.mode)
                          onThemeChanged()
                        }}
                      >
                        {option.icon}
                        <span>{option.label}</span>
                      </button>
                    ))}
                  </div>
                </div>

                <div className={modelsCss.rowCard} style={{ marginTop: 8 }}>
                  <div className={modelsCss.rowHead}>
                    <div className={modelsCss.rowIdentity}>
                      <span className={clsx(modelsCss.credentialDot, modelsCss.credentialDotConfigured)} />
                      <span className={modelsCss.rowName}>当前工作区目录</span>
                      <span className={modelsCss.rowTag}>Rust Workspace</span>
                    </div>
                  </div>
                  <div className={modelsCss.editorRoute} style={{ fontFamily: 'var(--ds-font-family-code)', wordBreak: 'break-all' }}>
                    {workspacePath || '连接中…'}
                  </div>
                </div>
              </div>
            )}

            {activeTab === 'models' && draft !== null && (
              <div className={modelsCss.section}>
                <h2 className={modelsCss.title}>模型与提供商</h2>
                <details className={modelsCss.rowCard}>
                  <summary>视觉服务降级 · {draft.visual_fallback_enabled ? '已启用' : '关闭'}</summary>
                  <label className={modelsCss.field}><span>仅当前角色明确不支持图片时启用配置的视觉服务</span><input type="checkbox" checked={draft.visual_fallback_enabled} onChange={e => updateDraft({ visual_fallback_enabled: e.target.checked })} /></label>
                  <label className={modelsCss.field}><span>视觉服务提供商</span><select className={modelsCss.input} value={draft.visual_provider} onChange={e => updateDraft({ visual_provider: e.target.value, visual_model: '' })}><option value="">请选择</option>{draft.providers.map(provider => <option key={provider._draftId} value={provider.id}>{provider.display_name || provider.id}</option>)}</select></label>
                  <label className={modelsCss.field}><span>支持图片的模型</span><select className={modelsCss.input} value={draft.visual_model} onChange={e => updateDraft({ visual_model: e.target.value })}><option value="">请选择</option>{Object.entries(draft.providers.find(p => p.id === draft.visual_provider)?.model_capabilities ?? {}).filter(([, cap]) => cap.image_input === 'supported').map(([model]) => <option key={model} value={model}>{model}</option>)}</select></label>
                </details>
                <p className={modelsCss.intro}>
                  配置模型提供商端点、API 协议及模型目录。API 密钥安全存储在本地凭据文件中，不会在前端回显。
                </p>

                {/* Primary Orchestrator Route Card */}
                <div className={modelsCss.setupCard}>
                  <div className={modelsCss.editorHeader}>
                    <span className={modelsCss.editorTitle}>默认主代理路由</span>
                    <span className={modelsCss.editorRoute}>新会话与主任务默认使用的提供商和模型</span>
                  </div>
                  <div className={modelsCss.modelAdvanced}>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>主代理提供商</span>
                      <select
                        className={clsx(modelsCss.input, modelsCss.selectInput)}
                        style={{ maxWidth: '100%' }}
                        value={draft.orchestrator_provider}
                        onChange={(e) => {
                          const nextProviderId = e.target.value
                          const prov = draft.providers.find(p => p.id === nextProviderId)
                          const firstModel = prov ? parseModels(prov.models_text)[0] ?? '' : ''
                          const capability = prov?.model_capabilities[firstModel]
                          updateDraft({
                            orchestrator_provider: nextProviderId,
                            orchestrator_model: firstModel,
                            reasoning_effort: capability?.default_effort ?? '',
                            fast_mode: false,
                          })
                        }}
                      >
                        <option value="">未选择提供商</option>
                        {draft.providers.map(p => (
                          <option key={p._draftId} value={p.id}>
                            {p.display_name || p.id} ({p.id})
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>主代理模型 ID</span>
                      <input
                        className={modelsCss.input}
                        list="dsh-main-model-list"
                        placeholder="输入或选择模型 ID"
                        value={draft.orchestrator_model}
                        onChange={(e) => {
                          const model = e.target.value
                          const capability = mainProviderObj?.model_capabilities[model]
                          updateDraft({
                            orchestrator_model: model,
                            reasoning_effort: capability?.default_effort ?? '',
                            fast_mode: false,
                          })
                        }}
                      />
                      <datalist id="dsh-main-model-list">
                        {mainModels.map(m => <option key={m} value={m} />)}
                      </datalist>
                    </label>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>思考等级</span>
                      <select
                        className={clsx(modelsCss.input, modelsCss.selectInput)}
                        value={mainCapability?.reasoning_efforts.includes(draft.reasoning_effort) ? draft.reasoning_effort : ''}
                        disabled={!mainCapability?.reasoning_efforts.length}
                        onChange={(e) => { updateDraft({ reasoning_effort: e.target.value }) }}
                      >
                        <option value="">模型默认</option>
                        {(mainCapability?.reasoning_efforts ?? []).map(level => (
                          <option key={level} value={level}>{level}</option>
                        ))}
                      </select>
                    </label>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>⚡ 请求加速</span>
                      <input
                        type="checkbox"
                        checked={Boolean(mainCapability?.fast_mode && draft.fast_mode)}
                        disabled={!mainCapability?.fast_mode}
                        onChange={(e) => { updateDraft({ fast_mode: e.target.checked }) }}
                      />
                      <span className={modelsCss.editorRoute}>代理是否实际加速，以响应返回的服务档位为准。</span>
                    </label>
                  </div>
                </div>

                {/* Provider List */}
                <ul className={modelsCss.rows}>
                  {draft.providers.map((provider) => {
                    const isOpen = openProviderId === provider._draftId
                    const configured = (Boolean(provider.api_key) || provider.has_api_key) && !provider.clear_api_key
                    const modelsCount = parseModels(provider.models_text).length
                    const candidates = discoveredModels[provider._draftId] ?? []
                    const activeModelsSet = new Set(parseModels(provider.models_text))

                    return (
                      <li key={provider._draftId} className={modelsCss.rowCard}>
                        <div className={modelsCss.rowHead}>
                          <div className={modelsCss.rowIdentity}>
                            <span
                              className={clsx(
                                modelsCss.credentialDot,
                                configured ? modelsCss.credentialDotConfigured : modelsCss.credentialDotMissing,
                              )}
                              title={configured ? '密钥已配置' : '未配置密钥'}
                            />
                            <span className={modelsCss.rowName}>
                              {provider.display_name || provider.id || '新提供商'}
                            </span>
                            <span className={modelsCss.rowTag}>{protocolTag(provider.api_type)}</span>
                            <span className={modelsCss.editorRoute}>
                              {provider.url || '未设置地址'} · {modelsCount} 个模型
                            </span>
                          </div>
                          <div className={modelsCss.rowActions}>
                            <button
                              type="button"
                              className={modelsCss.secondaryButton}
                              onClick={() => { setOpenProviderId(isOpen ? null : provider._draftId) }}
                            >
                              {isOpen ? '收起' : '编辑'}
                            </button>
                            <button
                              type="button"
                              className={modelsCss.dangerButton}
                              onClick={() => { handleDeleteProvider(provider) }}
                            >
                              删除
                            </button>
                          </div>
                        </div>

                        {isOpen && (
                          <div className={modelsCss.editor}>
                            <div className={modelsCss.modelAdvanced}>
                              <label className={modelsCss.field}>
                                <span className={modelsCss.fieldLabel}>Provider ID</span>
                                <input
                                  className={modelsCss.input}
                                  readOnly={!provider._new}
                                  placeholder="例如 deepseek"
                                  value={provider.id}
                                  onChange={(e) => { updateProvider(provider._draftId, { id: e.target.value }) }}
                                />
                              </label>
                              <label className={modelsCss.field}>
                                <span className={modelsCss.fieldLabel}>显示名称</span>
                                <input
                                  className={modelsCss.input}
                                  placeholder="例如 DeepSeek Official"
                                  value={provider.display_name}
                                  onChange={(e) => { updateProvider(provider._draftId, { display_name: e.target.value }) }}
                                />
                              </label>
                            </div>

                            <div className={modelsCss.modelAdvanced}>
                              <label className={modelsCss.field}>
                                <span className={modelsCss.fieldLabel}>API 基础地址 (Base URL)</span>
                                <input
                                  className={modelsCss.input}
                                  placeholder="https://api.deepseek.com/v1"
                                  value={provider.url}
                                  onChange={(e) => { updateProvider(provider._draftId, { url: e.target.value }) }}
                                />
                              </label>
                              <label className={modelsCss.field}>
                                <span className={modelsCss.fieldLabel}>接口协议</span>
                                <select
                                  className={clsx(modelsCss.input, modelsCss.selectInput)}
                                  style={{ maxWidth: '100%' }}
                                  value={provider.api_type}
                                  onChange={(e) => {
                                    updateProvider(provider._draftId, { api_type: e.target.value as ProviderApiType })
                                  }}
                                >
                                  <option value="openai-completions">OpenAI Chat Completions</option>
                                  <option value="openai-responses">OpenAI Responses</option>
                                  <option value="anthropic-messages">Anthropic Messages</option>
                                </select>
                              </label>
                            </div>

                            <label className={modelsCss.field}>
                              <span className={modelsCss.fieldLabel}>
                                API 密钥
                                {provider.has_api_key && !provider.clear_api_key && (
                                  <button
                                    type="button"
                                    className={modelsCss.linkButton}
                                    onClick={() => { updateProvider(provider._draftId, { clear_api_key: true, api_key: '' }) }}
                                  >
                                    清除已存密钥
                                  </button>
                                )}
                              </span>
                              <input
                                type="password"
                                className={modelsCss.input}
                                placeholder={
                                  provider.clear_api_key
                                    ? '保存后将清除已存密钥'
                                    : provider.has_api_key
                                      ? '已配置密钥（留空保持不变，输入新值则覆盖）'
                                      : 'sk-...'
                                }
                                value={provider.api_key}
                                onChange={(e) => {
                                  updateProvider(provider._draftId, { api_key: e.target.value, clear_api_key: false })
                                }}
                              />
                            </label>

                            <div className={modelsCss.modelCatalog}>
                              <div className={modelsCss.modelListHead}>
                                <div className={modelsCss.modelCatalogHeading}>
                                  <span className={modelsCss.modelCatalogTitle}>模型目录（每行一个模型 ID）</span>
                                  <span className={modelsCss.modelCatalogMeta}>
                                    可手动输入或从上游 `/v1/models` 端点自动拉取可用模型。
                                  </span>
                                </div>
                                <button
                                  type="button"
                                  className={modelsCss.addModelButton}
                                  disabled={discoveringId === provider._draftId}
                                  onClick={() => { void handleDiscover(provider) }}
                                >
                                  <IconPlusOutlineRegular size={12} />
                                  {discoveringId === provider._draftId ? '正在查询…' : '从端点拉取模型'}
                                </button>
                              </div>

                              <textarea
                                className={modelsCss.input}
                                style={{ height: 92, padding: '8px 10px', fontFamily: 'var(--ds-font-family-code)', resize: 'vertical' }}
                                placeholder="deepseek-chat&#10;deepseek-reasoner"
                                value={provider.models_text}
                                onChange={(e) => { updateProvider(provider._draftId, { models_text: e.target.value }) }}
                              />

                              <div className={modelsCss.modelCatalogMeta}>按提供方声明模型能力；模型 ID 不代表支持图片，未声明时为未知。</div>
                              {parseModels(provider.models_text).map(model => {
                                const capability = provider.model_capabilities[model] ?? { reasoning_efforts: [], default_effort: null, fast_mode: false }
                                return (
                                  <details key={model} className={modelsCss.rowCard}>
                                    <summary>{model} · 思考等级 {capability.reasoning_efforts.length ? capability.reasoning_efforts.join(', ') : '未声明'}</summary>
                                    <div className={modelsCss.modelAdvanced}>
                                      <label className={modelsCss.field}>
                                        <span className={modelsCss.fieldLabel}>图片输入能力</span>
                                        <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={capability.image_input ?? 'unknown'} onChange={event => { updateModelCapability(provider, model, { image_input: event.target.value as 'supported' | 'unsupported' | 'unknown' }) }}>
                                          <option value="unknown">未知：保留材料，报告限制</option><option value="supported">支持：发送真实图片</option><option value="unsupported">不支持</option>
                                        </select>
                                      </label>
                                      <div className={modelsCss.field}>
                                        <span className={modelsCss.fieldLabel}>支持的等级</span>
                                        <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
                                          {REASONING_LEVELS.map(level => (
                                            <label key={level}>
                                              <input type="checkbox" checked={capability.reasoning_efforts.includes(level)} onChange={event => {
                                                const levels = event.target.checked
                                                  ? REASONING_LEVELS.filter(value => value === level || capability.reasoning_efforts.includes(value))
                                                  : capability.reasoning_efforts.filter(value => value !== level)
                                                updateModelCapability(provider, model, {
                                                  reasoning_efforts: levels,
                                                  default_effort: levels.includes(capability.default_effort ?? '') ? capability.default_effort : null,
                                                })
                                              }} /> {level}
                                            </label>
                                          ))}
                                        </div>
                                      </div>
                                      <label className={modelsCss.field}>
                                        <span className={modelsCss.fieldLabel}>默认等级</span>
                                        <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={capability.default_effort ?? ''} onChange={event => { updateModelCapability(provider, model, { default_effort: event.target.value || null }) }}>
                                          <option value="">模型默认</option>
                                          {capability.reasoning_efforts.map(level => <option key={level} value={level}>{level}</option>)}
                                        </select>
                                      </label>
                                      <label className={modelsCss.field}>
                                        <span className={modelsCss.fieldLabel}>支持请求加速</span>
                                        <input type="checkbox" checked={capability.fast_mode} onChange={event => { updateModelCapability(provider, model, { fast_mode: event.target.checked }) }} />
                                      </label>
                                    </div>
                                  </details>
                                )
                              })}

                              {candidates.length > 0 && (
                                <div className={modelsCss.rowCard} style={{ background: 'var(--dsw-alias-bg-layer-1)' }}>
                                  <div className={modelsCss.candidateToolbar}>
                                    <input
                                      type="search"
                                      className={clsx(modelsCss.input, modelsCss.candidateSearch)}
                                      placeholder={`筛选发现的 ${candidates.length} 个模型…`}
                                      value={discoverFilter}
                                      onChange={(e) => { setDiscoverFilter(e.target.value) }}
                                    />
                                  </div>
                                  <ul className={modelsCss.candidateList}>
                                    {candidates
                                      .filter(id => id.toLowerCase().includes(discoverFilter.toLowerCase()))
                                      .map(id => (
                                        <li key={id} className={modelsCss.candidate}>
                                          <label className={modelsCss.candidateLabel}>
                                            <input
                                              type="checkbox"
                                              checked={activeModelsSet.has(id)}
                                              onChange={() => { toggleCandidateModel(provider, id) }}
                                            />
                                            <span className={modelsCss.candidateId}>{id}</span>
                                          </label>
                                        </li>
                                      ))}
                                  </ul>
                                </div>
                              )}
                            </div>
                          </div>
                        )}
                      </li>
                    )
                  })}
                </ul>

                <div className={modelsCss.addActions}>
                  <button type="button" className={modelsCss.addButton} onClick={handleAddProvider}>
                    <IconPlusOutlineRegular size={16} />
                    <span>添加模型提供商</span>
                  </button>
                </div>
              </div>
            )}

            {activeTab === 'subagent' && draft !== null && (
              <div className={modelsCss.section}>
                <h2 className={modelsCss.title}>子代理 (Subagent)</h2>
                <p className={modelsCss.intro}>
                  允许主代理将独立子任务委派给子代理并行深入调查或修改，并在子代理完成后汇总结果。
                </p>

                <div className={modelsCss.rowCard}>
                  <div className={modelsCss.rowHead}>
                    <div className={modelsCss.rowIdentity}>
                      <span className={modelsCss.rowName}>启用子代理自主委派</span>
                      <span className={modelsCss.rowTag}>Subagent</span>
                    </div>
                    <div className={modelsCss.rowActions}>
                      <Switch
                        checked={draft.enable_subagent}
                        label="启用子代理"
                        onChange={(checked) => { updateDraft({ enable_subagent: checked }) }}
                      />
                    </div>
                  </div>
                  <p className={modelsCss.advancedHint}>
                    启用后，主代理可通过 `spawn_subagent` 工具派生子任务；你也可以在会话视图与轨迹视图中随时检视子代理步骤。
                  </p>
                </div>

                <div className={clsx(subagentCss.section, modelsCss.setupCard)}>
                  <div className={modelsCss.editorHeader}>
                    <span className={modelsCss.editorTitle}>子代理专用模型（可选）</span>
                    <span className={modelsCss.editorRoute}>留空时默认继承主代理提供商与模型</span>
                  </div>
                  <div className={modelsCss.modelAdvanced}>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>子代理提供商</span>
                      <select
                        className={clsx(modelsCss.input, modelsCss.selectInput)}
                        style={{ maxWidth: '100%' }}
                        value={draft.expert_provider}
                        onChange={(e) => {
                          const nextProviderId = e.target.value
                          const prov = draft.providers.find(p => p.id === nextProviderId)
                          const firstModel = prov ? parseModels(prov.models_text)[0] ?? '' : ''
                          updateDraft({
                            expert_provider: nextProviderId,
                            expert_model: firstModel,
                          })
                        }}
                      >
                        <option value="">跟随主代理 ({draft.orchestrator_provider || '未设置'})</option>
                        {draft.providers.map(p => (
                          <option key={p._draftId} value={p.id}>
                            {p.display_name || p.id} ({p.id})
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>子代理模型 ID</span>
                      <input
                        className={modelsCss.input}
                        list="dsh-expert-model-list"
                        placeholder={draft.orchestrator_model || '跟随主代理模型'}
                        value={draft.expert_model}
                        onChange={(e) => { updateDraft({ expert_model: e.target.value }) }}
                      />
                      <datalist id="dsh-expert-model-list">
                        {expertModels.map(m => <option key={m} value={m} />)}
                      </datalist>
                    </label>
                  </div>
                </div>
              </div>
            )}

            {activeTab === 'observer' && draft !== null && (
              <div className={modelsCss.section}>
                <h2 className={modelsCss.title}>Observer 全局观察</h2>
                <p className={modelsCss.intro}>
                  Observer 使用独立上下文并持续掌握 Worker 的计划与进度：计划形成或路径推进到关键阶段时，给出非阻断的方向建议；Worker 也可以主动询问历史决策，避免重复回头查找。任务结束后，Observer 复盘完整路径、总结可缩短之处，并把有复用价值的工作信息、相关性和后续关注点写入工程记忆。计划、操作和技术判断仍由 Worker 自己负责。
                </p>

                <div className={modelsCss.rowCard}>
                  <div className={modelsCss.rowHead}>
                    <div className={modelsCss.rowIdentity}>
                      <span className={modelsCss.rowName}>启用 Observer</span>
                      <span className={modelsCss.rowTag}>全局观察</span>
                    </div>
                    <div className={modelsCss.rowActions}>
                      <Switch
                        checked={draft.observer_enabled}
                        label="启用 Observer"
                        onChange={(checked) => { updateDraft({ observer_enabled: checked }) }}
                      />
                    </div>
                  </div>
                  <p className={modelsCss.advancedHint}>
                    Observer 只观察工作方向与过程效率，不检查语法、编译错误或普通工具报错，也不会批准、中断或接管工作。它暂时不可用时，Worker 照常继续；任务结束复盘会增加一次 Observer 模型调用。
                  </p>
                </div>

                <div className={clsx(subagentCss.section, modelsCss.setupCard)}>
                  <div className={modelsCss.editorHeader}>
                    <span className={modelsCss.editorTitle}>Observer 模型</span>
                    <span className={modelsCss.editorRoute}>默认使用当前任务 Worker 的模型，也可单独选择模型路由</span>
                  </div>
                  <div className={modelsCss.modelAdvanced}>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>模型提供商</span>
                      <select
                        className={clsx(modelsCss.input, modelsCss.selectInput)}
                        style={{ maxWidth: '100%' }}
                        value={draft.observer_provider}
                        onChange={(e) => {
                          const nextProviderId = e.target.value
                          const provider = draft.providers.find(p => p.id === nextProviderId)
                          const firstModel = provider ? parseModels(provider.models_text)[0] ?? '' : ''
                          updateDraft({ observer_provider: nextProviderId, observer_model: firstModel })
                        }}
                      >
                        <option value="">跟随 Worker（{draft.orchestrator_provider || '主模型未设置'}）</option>
                        {draft.providers.filter(p => p.api_type === 'openai-completions').map(p => (
                          <option key={p._draftId} value={p.id}>
                            {p.display_name || p.id} ({p.id})
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className={modelsCss.field}>
                      <span className={modelsCss.fieldLabel}>模型 ID</span>
                      <input
                        className={modelsCss.input}
                        list="dsh-observer-model-list"
                        placeholder={draft.observer_provider ? '选择或输入该提供商中的模型 ID' : (draft.orchestrator_model || '跟随 Worker 模型')}
                        value={draft.observer_model}
                        disabled={!draft.observer_provider}
                        onChange={(e) => { updateDraft({ observer_model: e.target.value }) }}
                      />
                      <datalist id="dsh-observer-model-list">
                        {observerModels.map(m => <option key={m} value={m} />)}
                      </datalist>
                    </label>
                  </div>
                  <p className={modelsCss.advancedHint}>
                    单独选择时会复用该提供商已保存的 URL 和 API Key；提供商与模型需同时配置。当前任务开始后，模型路由会写入轨迹。
                  </p>
                </div>
              </div>
            )}

            {activeTab === 'plugins' && (
              <div className={pluginsCss.section}>
                <div className={modelsCss.modelListHead}>
                  <div>
                    <h2 className={modelsCss.title}>Cordis 插件树</h2>
                    <p className={modelsCss.intro}>检视 Rust 运行时与浏览器 Cordis 挂载的插件实例、依赖服务与生命周期事件。</p>
                  </div>
                  <button type="button" className={modelsCss.secondaryButton} onClick={loadPlugins}>
                    刷新状态
                  </button>
                </div>

                <div className={pluginsCss.search}>
                  <IconSearchOutlineRegular size={16} />
                  <input
                    type="search"
                    placeholder="搜索插件 ID 或服务名…"
                    value={pluginQuery}
                    onChange={(e) => { setPluginQuery(e.target.value) }}
                  />
                </div>

                <div className={pluginsCss.catalog}>
                  <div className={pluginsCss.catalogHeading}>
                    <h3>Rust Cordis 插件</h3>
                    <span>{pluginsData?.plugins.length ?? 0}</span>
                  </div>
                  <ul className={pluginsCss.cards}>
                    {(pluginsData?.plugins ?? [])
                      .filter(p =>
                        p.id.toLowerCase().includes(pluginQuery.toLowerCase())
                        || p.provides.some(s => s.toLowerCase().includes(pluginQuery.toLowerCase())))
                      .map((plugin) => {
                        const cardKey = `${plugin.scope}:${plugin.id}`
                        const isOpen = Boolean(expandedPlugins[cardKey])
                        return (
                          <li key={cardKey} className={pluginsCss.card} data-open={isOpen ? 'true' : undefined}>
                            <button
                              type="button"
                              className={pluginsCss.cardContent}
                              onClick={() => {
                                setExpandedPlugins(prev => ({ ...prev, [cardKey]: !isOpen }))
                              }}
                            >
                              <div className={pluginsCss.cardMainRow}>
                                <span className={pluginsCss.cardTitle}>{plugin.id}</span>
                                <span className={pluginsCss.cardTrailing}>
                                  <span className={pluginsCss.phaseDot}>
                                    <StateDot state={plugin.status === 'ready' || plugin.status === 'active' ? 'done' : 'ongoing'} size={6} />
                                  </span>
                                  <IconChevronDownOutlineRegular className={pluginsCss.chevron} size={12} />
                                </span>
                              </div>
                              <span className={pluginsCss.cardIdentity}>
                                scope #{plugin.scope} · {plugin.status}
                              </span>
                            </button>
                            {isOpen && (
                              <div className={pluginsCss.cardDetails}>
                                <dl className={pluginsCss.details}>
                                  <div>
                                    <dt>提供服务</dt>
                                    <dd>{plugin.provides.length ? plugin.provides.join(', ') : '—'}</dd>
                                  </div>
                                  <div>
                                    <dt>依赖服务</dt>
                                    <dd>
                                      {plugin.requires.length
                                        ? plugin.requires.map(r => `${r.service} (#${r.provider_scope})`).join(', ')
                                        : '—'}
                                    </dd>
                                  </div>
                                </dl>
                              </div>
                            )}
                          </li>
                        )
                      })}
                  </ul>
                </div>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>,
    document.body,
  )
}
