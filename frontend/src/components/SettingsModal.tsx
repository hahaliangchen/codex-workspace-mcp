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
  StateDot,
  Switch,
} from '@deepseek-ai/dsh-client-ui-primitives'
import {
  api,
  REASONING_LEVELS,
  type ModelCapabilities,
  type ModelUseSettings,
  type PluginsSnapshot,
  type ProviderApiType,
  type SettingsData,
  type SkillPackage,
  type ModelRef,
} from '../api.ts'
import appearanceCss from '../dsh/settings/AppearanceRow.module.css'
import modelsCss from '../dsh/settings/ModelsSection.module.css'
import pluginsCss from '../dsh/settings/PluginInventorySettingsTab.module.css'
import rootCss from '../dsh/settings/SettingsRoot.module.css'
import { brandIcon, getPalette, getThemeMode, setPalette, setThemeMode, type ThemeMode, type ThemePalette } from '../theme.ts'
import type { SettingsTab } from '../useAgentWorkspace.ts'

interface DraftProvider {
  prefix_system_messages:boolean
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
  plugin_models: Record<string,readonly ModelRef[]>
  visual_tool: ModelUseSettings
  delegate_agent: ModelUseSettings
  revision: string
  providers: DraftProvider[]
  orchestrator_provider: string
  orchestrator_model: string
  reasoning_effort: string
  fast_mode: boolean
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
    plugin_models: {...data.plugin_models},
    visual_tool: data.visual_tool ?? {},
    delegate_agent: data.delegate_agent ?? {},
    providers: data.providers.map((p, index) => ({
      _draftId: `saved-${index}-${p.id}`,
      _new: false,
      id: p.id,
      display_name: p.display_name ?? '',
      url: p.url,
      api_type: p.api_type,
      prefix_system_messages:Boolean(p.prefix_system_messages),
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
  const [palette, setPaletteState] = useState<ThemePalette>(getPalette)
  const [draft, setDraft] = useState<DraftSettings | null>(null)
  const [openProviderId, setOpenProviderId] = useState<string | null>(null)
  const [baseline, setBaseline] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [probingImage, setProbingImage] = useState<string | null>(null)
  const [statusMsg, setStatusMsg] = useState<{ text: string; error: boolean } | null>(null)
  const [discoveringId, setDiscoveringId] = useState<string | null>(null)
  const [discoveredModels, setDiscoveredModels] = useState<Record<string, string[]>>({})
  const [discoverFilter, setDiscoverFilter] = useState('')
  const [pluginsData, setPluginsData] = useState<PluginsSnapshot | null>(null)
  const [packages,setPackages]=useState<SkillPackage[]>([])
  const [pluginQuery, setPluginQuery] = useState('')
  const [expandedPlugins, setExpandedPlugins] = useState<Record<string, boolean>>({})

  const loadSettings = useCallback(() => {
    setStatusMsg(null)
    api.getSettings().then(
      (data) => {
        const next = toDraft(data)
        setDraft(next)
        setBaseline(JSON.stringify(next))
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
    void api.skillPackages().then(data=>setPackages(data.packages)).catch(()=>{})
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
    setPaletteState(getPalette())
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
          prefix_system_messages:p.prefix_system_messages,
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
        plugin_models: draft.plugin_models,
        visual_tool: draft.visual_tool,
        delegate_agent: draft.delegate_agent,
        providers: providersPayload,
        orchestrator_provider: draft.orchestrator_provider || null,
        orchestrator_model: draft.orchestrator_model || null,
        reasoning_effort: selectedCapability?.reasoning_efforts.includes(draft.reasoning_effort) ? draft.reasoning_effort : '',
        fast_mode: Boolean(selectedCapability?.fast_mode && draft.fast_mode),
      })
      const savedDraft = toDraft(saved)
      setDraft(savedDraft)
      setBaseline(JSON.stringify(savedDraft))
      setStatusMsg({ text: '设置已保存并立即对新任务生效。', error: false })
      onSaved()
      return saved
    } catch (err) {
      setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
    } finally {
      setSaving(false)
    }
  }

  const handleProbeImage = async (provider: DraftProvider, model: string) => {
    if (probingImage || saving) return
    setProbingImage(`${provider._draftId}:${model}`)
    try {
      const saved = await handleSave()
      if (!saved) return
      setStatusMsg({ text: '正在验证图片输入…', error: false })
      const result = await api.probeImage({ revision: saved.revision, provider: provider.id.trim(), model })
      const verified = result.settings.providers.find(p => p.id === provider.id.trim())?.model_capabilities[model]
      if (!verified) throw new Error('验证返回缺少模型能力记录')
      setDraft(current => {
        if (!current) return current
        const next = { ...current, revision: result.settings.revision,
          providers: current.providers.map(item => item.id.trim() === provider.id.trim()
            ? { ...item, model_capabilities: { ...item.model_capabilities, [model]: verified } }
            : item) }
        setBaseline(JSON.stringify(next))
        return next
      })
      const capability = result.probe.capability
      setStatusMsg({ text: capability === 'supported' ? '模型正确识别了测试图片，已接通真实图片输入。'
        : capability === 'unsupported' ? '接口明确拒绝图片，已保存原始原因。'
        : `图片能力仍未确认：${String(result.probe.error ?? '模型未正确识别测试图片')}`, error: capability !== 'supported' })
      onSaved()
    } catch (err) {
      setStatusMsg({ text: err instanceof Error ? err.message : String(err), error: true })
    } finally { setProbingImage(null) }
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
      prefix_system_messages:false,
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

  if (!open) return null

  const dirty = draft !== null && baseline !== null && JSON.stringify(draft) !== baseline

  const navItems: readonly { id: SettingsTab; label: string; icon: JSX.Element }[] = [
    { id: 'general', label: '通用', icon: <IconSettingsOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'models', label: '模型与提供商', icon: <IconDatabaseOutlineRegular className={rootCss.navIcon} size={18} /> },
    { id: 'tools', label: '专用模型', icon: <IconCodeOutlineRegular className={rootCss.navIcon} size={18} /> },
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
              {dirty && (
                <button
                  type="button"
                  className={modelsCss.primaryButton}
                  disabled={saving || probingImage !== null || draft === null}
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

            {draft === null && statusMsg === null && (activeTab === 'models' || activeTab === 'tools') && (
              <p className={modelsCss.intro}>正在加载设置…</p>
            )}

            {activeTab === 'general' && (
              <div className={modelsCss.section}>
                <div className={modelsCss.pageHead}>
                  <h2 className={modelsCss.title}>通用设置</h2>
                  <p className={modelsCss.intro}>自定义界面的外观主题，并查看当前工作区环境。</p>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>外观</div>
                  <div className={appearanceCss.group}>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>外观模式</span>
                        <span className={appearanceCss.rowHint}>浅色、深色，或跟随系统自动切换</span>
                      </div>
                      <div className={appearanceCss.segmented} role="radiogroup" aria-label="外观模式">
                        {([
                          { mode: 'light', label: '浅色', icon: <IconLightOutlineRegular size={16} /> },
                          { mode: 'dark', label: '深色', icon: <IconDarkOutlineRegular size={16} /> },
                          { mode: 'system', label: '跟随系统', icon: <IconSettingsOutlineRegular size={16} /> },
                        ] as const).map(option => (
                          <button
                            key={option.mode}
                            type="button"
                            role="radio"
                            aria-checked={themeMode === option.mode}
                            className={clsx(appearanceCss.segment, themeMode === option.mode && appearanceCss.segmentOn)}
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
                  </div>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>主题色</div>
                  <div className={appearanceCss.group} role="radiogroup" aria-label="主题色">
                    {([
                      { palette: 'sprout', label: '新芽', hint: '生机 · 青绿', swatches: ['#1d9e75', '#5dcaa5', '#e3f1ea'] },
                      { palette: 'ginkgo', label: '银杏', hint: '秋日 · 清爽冷静 · 枫叶红与银杏黄', swatches: ['#b5452c', '#e2a52e', '#f8eccd'] },
                    ] as const).map(option => (
                      <button
                        key={option.palette}
                        type="button"
                        role="radio"
                        aria-checked={palette === option.palette}
                        className={clsx(appearanceCss.row, appearanceCss.rowButton)}
                        onClick={() => {
                          setPaletteState(option.palette)
                          setPalette(option.palette)
                        }}
                      >
                        <img className={appearanceCss.brand} src={brandIcon(option.palette)} width={28} height={28} alt="" />
                        <div className={appearanceCss.rowText}>
                          <span className={appearanceCss.rowTitle}>{option.label}</span>
                          <span className={appearanceCss.rowHint}>{option.hint}</span>
                        </div>
                        <span className={appearanceCss.swatches} aria-hidden="true">
                          {option.swatches.map(color => <i key={color} style={{ background: color }} />)}
                        </span>
                        <span className={clsx(appearanceCss.check, palette === option.palette && appearanceCss.checkOn)} aria-hidden="true" />
                      </button>
                    ))}
                  </div>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>工作区</div>
                  <div className={appearanceCss.group}>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>当前工作区目录</span>
                        <span className={appearanceCss.rowHint}>Rust Workspace</span>
                      </div>
                      <span className={appearanceCss.path} title={workspacePath}>{workspacePath || '连接中…'}</span>
                    </div>
                  </div>
                </div>
              </div>
            )}

            {activeTab === 'models' && draft !== null && (
              <div className={modelsCss.section}>
                <div className={modelsCss.pageHead}>
                  <h2 className={modelsCss.title}>模型与提供商</h2>
                  <p className={modelsCss.intro}>
                    配置模型提供商端点、API 协议及模型目录。API 密钥安全存储在本地凭据文件中，不会在前端回显。
                  </p>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>默认主代理</div>
                  <div className={appearanceCss.group}>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>提供商</span>
                        <span className={appearanceCss.rowHint}>新会话与主任务默认使用的提供商</span>
                      </div>
                      <div className={appearanceCss.control}>
                        <select
                          className={clsx(modelsCss.input, modelsCss.selectInput)}
                          aria-label="主代理提供商"
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
                      </div>
                    </div>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>模型 ID</span>
                        <span className={appearanceCss.rowHint}>可输入，也可从该提供商的目录中选择</span>
                      </div>
                      <div className={appearanceCss.control}>
                        <input
                          className={modelsCss.input}
                          aria-label="主代理模型 ID"
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
                      </div>
                    </div>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>思考等级</span>
                        <span className={appearanceCss.rowHint}>可选等级取决于模型声明的能力</span>
                      </div>
                      <div className={appearanceCss.control}>
                        <select
                          className={clsx(modelsCss.input, modelsCss.selectInput)}
                          aria-label="思考等级"
                          value={mainCapability?.reasoning_efforts.includes(draft.reasoning_effort) ? draft.reasoning_effort : ''}
                          disabled={!mainCapability?.reasoning_efforts.length}
                          onChange={(e) => { updateDraft({ reasoning_effort: e.target.value }) }}
                        >
                          <option value="">模型默认</option>
                          {(mainCapability?.reasoning_efforts ?? []).map(level => (
                            <option key={level} value={level}>{level}</option>
                          ))}
                        </select>
                      </div>
                    </div>
                    <div className={appearanceCss.row}>
                      <div className={appearanceCss.rowText}>
                        <span className={appearanceCss.rowTitle}>⚡ 请求加速</span>
                        <span className={appearanceCss.rowHint}>
                          {mainCapability?.fast_mode ? '代理是否实际加速，以响应返回的服务档位为准。' : '当前模型不支持请求加速。'}
                        </span>
                      </div>
                      <Switch
                        checked={Boolean(mainCapability?.fast_mode && draft.fast_mode)}
                        disabled={!mainCapability?.fast_mode}
                        label="请求加速"
                        onChange={(checked) => { updateDraft({ fast_mode: checked }) }}
                      />
                    </div>
                  </div>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>提供商</div>
                  <div className={appearanceCss.group}>
                    {draft.providers.map((provider) => {
                      const isOpen = openProviderId === provider._draftId
                      const configured = (Boolean(provider.api_key) || provider.has_api_key) && !provider.clear_api_key
                      const modelsCount = parseModels(provider.models_text).length
                      const candidates = discoveredModels[provider._draftId] ?? []
                      const activeModelsSet = new Set(parseModels(provider.models_text))

                      return (
                        <div key={provider._draftId} className={appearanceCss.item}>
                          <div className={appearanceCss.row}>
                            <span
                              className={clsx(appearanceCss.status, configured ? appearanceCss.statusOn : appearanceCss.statusOff)}
                              title={configured ? '密钥已配置' : '未配置密钥'}
                            />
                            <div className={appearanceCss.rowText}>
                              <span className={appearanceCss.rowTitle}>
                                {provider.display_name || provider.id || '新提供商'}
                                {' '}
                                <span className={appearanceCss.tag}>{protocolTag(provider.api_type)}</span>
                              </span>
                              <span className={clsx(appearanceCss.rowHint, appearanceCss.ellipsis)}>
                                {provider.url || '未设置地址'} · {modelsCount} 个模型
                              </span>
                            </div>
                            <button
                              type="button"
                              className={appearanceCss.link}
                              onClick={() => { setOpenProviderId(isOpen ? null : provider._draftId) }}
                            >
                              {isOpen ? '收起' : '编辑'}
                            </button>
                            <button
                              type="button"
                              className={clsx(appearanceCss.link, appearanceCss.linkDanger)}
                              onClick={() => { handleDeleteProvider(provider) }}
                            >
                              删除
                            </button>
                          </div>

                          {isOpen && (
                            <div className={appearanceCss.expand}>
                              <div className={appearanceCss.row}>
                                <span className={appearanceCss.rowTitle}>Provider ID</span>
                                <div className={appearanceCss.control}>
                                  <input
                                    className={modelsCss.input}
                                    readOnly={!provider._new}
                                    placeholder="例如 deepseek"
                                    value={provider.id}
                                    onChange={(e) => { updateProvider(provider._draftId, { id: e.target.value }) }}
                                  />
                                </div>
                              </div>
                              <div className={appearanceCss.row}>
                                <span className={appearanceCss.rowTitle}>显示名称</span>
                                <div className={appearanceCss.control}>
                                  <input
                                    className={modelsCss.input}
                                    placeholder="例如 DeepSeek Official"
                                    value={provider.display_name}
                                    onChange={(e) => { updateProvider(provider._draftId, { display_name: e.target.value }) }}
                                  />
                                </div>
                              </div>
                              <div className={appearanceCss.row}>
                                <span className={appearanceCss.rowTitle}>API 基础地址</span>
                                <div className={appearanceCss.control}>
                                  <input
                                    className={modelsCss.input}
                                    placeholder="https://api.deepseek.com/v1"
                                    value={provider.url}
                                    onChange={(e) => { updateProvider(provider._draftId, { url: e.target.value }) }}
                                  />
                                </div>
                              </div>
                              <div className={appearanceCss.row}>
                                <span className={appearanceCss.rowTitle}>接口协议</span>
                                <div className={appearanceCss.control}>
                                  <select
                                    className={clsx(modelsCss.input, modelsCss.selectInput)}
                                    value={provider.api_type}
                                    onChange={(e) => {
                                      updateProvider(provider._draftId, { api_type: e.target.value as ProviderApiType })
                                    }}
                                  >
                                    <option value="openai-completions">OpenAI Chat Completions</option>
                                    <option value="openai-responses">OpenAI Responses</option>
                                    <option value="anthropic-messages">Anthropic Messages</option>
                                  </select>
                                </div>
                              </div>
                              <div className={appearanceCss.row}>
                                <div className={appearanceCss.rowText}>
                                  <span className={appearanceCss.rowTitle}>系统消息集中到开头</span>
                                  <span className={appearanceCss.rowHint}>适用于要求此前缀的模型服务</span>
                                </div>
                                <Switch
                                  checked={provider.prefix_system_messages}
                                  label="系统消息集中到开头"
                                  onChange={(checked) => { updateProvider(provider._draftId, { prefix_system_messages: checked }) }}
                                />
                              </div>
                              <div className={appearanceCss.row}>
                                <div className={appearanceCss.rowText}>
                                  <span className={appearanceCss.rowTitle}>API 密钥</span>
                                  {provider.has_api_key && !provider.clear_api_key && (
                                    <button
                                      type="button"
                                      className={clsx(appearanceCss.link, appearanceCss.linkMuted)}
                                      style={{ alignSelf: 'flex-start', padding: 0 }}
                                      onClick={() => { updateProvider(provider._draftId, { clear_api_key: true, api_key: '' }) }}
                                    >
                                      清除已存密钥
                                    </button>
                                  )}
                                </div>
                                <div className={appearanceCss.control}>
                                  <input
                                    type="password"
                                    className={modelsCss.input}
                                    placeholder={
                                      provider.clear_api_key
                                        ? '保存后将清除已存密钥'
                                        : provider.has_api_key
                                          ? '已配置（留空保持不变）'
                                          : 'sk-...'
                                    }
                                    value={provider.api_key}
                                    onChange={(e) => {
                                      updateProvider(provider._draftId, { api_key: e.target.value, clear_api_key: false })
                                    }}
                                  />
                                </div>
                              </div>

                              <div className={clsx(appearanceCss.row, appearanceCss.rowStack)}>
                                <div className={appearanceCss.rowTop}>
                                  <div className={appearanceCss.rowText}>
                                    <span className={appearanceCss.rowTitle}>模型目录</span>
                                    <span className={appearanceCss.rowHint}>每行一个模型 ID，可手动输入或从上游 `/v1/models` 拉取。</span>
                                  </div>
                                  <button
                                    type="button"
                                    className={appearanceCss.link}
                                    disabled={discoveringId === provider._draftId}
                                    onClick={() => { void handleDiscover(provider) }}
                                  >
                                    {discoveringId === provider._draftId ? '正在查询…' : '从端点拉取'}
                                  </button>
                                </div>
                                <textarea
                                  className={appearanceCss.textarea}
                                  placeholder="deepseek-chat&#10;deepseek-reasoner"
                                  value={provider.models_text}
                                  onChange={(e) => { updateProvider(provider._draftId, { models_text: e.target.value }) }}
                                />
                              </div>

                              {candidates.length > 0 && (
                                <div className={clsx(appearanceCss.row, appearanceCss.rowStack)}>
                                  <input
                                    type="search"
                                    className={modelsCss.input}
                                    placeholder={`筛选发现的 ${candidates.length} 个模型…`}
                                    value={discoverFilter}
                                    onChange={(e) => { setDiscoverFilter(e.target.value) }}
                                  />
                                  <ul className={clsx(modelsCss.candidateList, appearanceCss.candidates)}>
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

                              {parseModels(provider.models_text).map(model => {
                                const capability = provider.model_capabilities[model] ?? { reasoning_efforts: [], default_effort: null, fast_mode: false }
                                return (
                                  <details key={model} className={appearanceCss.row} style={{ display: 'block', padding: 0 }}>
                                    <summary className={appearanceCss.modelSummary}>
                                      <span>{model}</span>
                                      <span className={appearanceCss.modelMeta}>
                                        思考等级 {capability.reasoning_efforts.length ? capability.reasoning_efforts.join(', ') : '未声明'}
                                      </span>
                                    </summary>
                                    <div className={appearanceCss.expand} style={{ margin: '0 8px 8px', background: 'var(--dsw-alias-bg-module-platform)' }}>
                                      <div className={appearanceCss.row}>
                                        <div className={appearanceCss.rowText}>
                                          <span className={appearanceCss.rowTitle}>图片输入能力</span>
                                          <span className={appearanceCss.rowHint}>图片支持需通过实际图片验证，验证会保存当前设置并发送一张随机测试图。</span>
                                        </div>
                                        <div className={appearanceCss.control}>
                                          <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={capability.image_input ?? 'unknown'} onChange={event => { updateModelCapability(provider, model, { image_input: event.target.value as 'supported' | 'unsupported' | 'unknown' }) }}>
                                            <option value="unknown">未知：保留材料，报告限制</option><option value="supported" disabled>已验证支持：发送真实图片</option><option value="unsupported">不支持</option>
                                          </select>
                                        </div>
                                      </div>
                                      <div className={appearanceCss.row}>
                                        <div className={appearanceCss.rowText}>
                                          <span className={appearanceCss.rowTitle}>验证图片输入</span>
                                          {capability.image_probe && (
                                            <span className={appearanceCss.rowHint}>
                                              上次：{capability.image_probe.status} · {capability.image_probe.elapsed_ms} ms · {new Date(capability.image_probe.checked_at).toLocaleString()}
                                              {capability.image_probe.error ? ` · ${capability.image_probe.error}` : ''}
                                            </span>
                                          )}
                                        </div>
                                        <button type="button" className={appearanceCss.link} disabled={saving || probingImage !== null || provider.api_type !== 'openai-completions'} onClick={() => { void handleProbeImage(provider, model) }}>
                                          {probingImage === `${provider._draftId}:${model}` ? '正在验证图片…' : '开始验证'}
                                        </button>
                                      </div>
                                      {capability.image_probe && (
                                        <details className={appearanceCss.row} style={{ display: 'block' }}>
                                          <summary className={appearanceCss.rowHint} style={{ cursor: 'pointer' }}>查看验证响应</summary>
                                          <pre style={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', margin: '8px 0 0', fontSize: 12 }}>{capability.image_probe.response_body}</pre>
                                        </details>
                                      )}
                                      <div className={clsx(appearanceCss.row, appearanceCss.rowStack)}>
                                        <span className={appearanceCss.rowTitle}>支持的思考等级</span>
                                        <div className={appearanceCss.chips}>
                                          {REASONING_LEVELS.map(level => (
                                            <label key={level} className={appearanceCss.chip}>
                                              <input type="checkbox" checked={capability.reasoning_efforts.includes(level)} onChange={event => {
                                                const levels = event.target.checked
                                                  ? REASONING_LEVELS.filter(value => value === level || capability.reasoning_efforts.includes(value))
                                                  : capability.reasoning_efforts.filter(value => value !== level)
                                                updateModelCapability(provider, model, {
                                                  reasoning_efforts: levels,
                                                  default_effort: levels.includes(capability.default_effort ?? '') ? capability.default_effort : null,
                                                })
                                              }} />
                                              {level}
                                            </label>
                                          ))}
                                        </div>
                                      </div>
                                      <div className={appearanceCss.row}>
                                        <span className={appearanceCss.rowTitle}>默认等级</span>
                                        <div className={appearanceCss.control}>
                                          <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={capability.default_effort ?? ''} onChange={event => { updateModelCapability(provider, model, { default_effort: event.target.value || null }) }}>
                                            <option value="">模型默认</option>
                                            {capability.reasoning_efforts.map(level => <option key={level} value={level}>{level}</option>)}
                                          </select>
                                        </div>
                                      </div>
                                      <div className={appearanceCss.row}>
                                        <span className={appearanceCss.rowTitle}>支持请求加速</span>
                                        <Switch
                                          checked={capability.fast_mode}
                                          label={`${model} 支持请求加速`}
                                          onChange={checked => { updateModelCapability(provider, model, { fast_mode: checked }) }}
                                        />
                                      </div>
                                    </div>
                                  </details>
                                )
                              })}
                            </div>
                          )}
                        </div>
                      )
                    })}
                    <button type="button" className={clsx(appearanceCss.row, appearanceCss.addRow)} onClick={handleAddProvider}>
                      <IconPlusOutlineRegular size={16} />
                      <span>添加模型提供商</span>
                    </button>
                  </div>
                </div>

                <div className={appearanceCss.block}>
                  <div className={appearanceCss.label}>插件可用模型</div>
                  <div className={appearanceCss.group}>
                    {[...packages,...Object.keys(draft.plugin_models).filter(id=>!packages.some(p=>p.id===id)).map(id=>({id,title:`${id}（尚未连接）`}))].map(plugin=>{
                      const selected=draft.plugin_models[plugin.id]
                      const setModels=(models:readonly ModelRef[]|undefined)=>{const next={...draft.plugin_models};if(models===undefined)delete next[plugin.id];else next[plugin.id]=models;updateDraft({plugin_models:next})}
                      return <div key={plugin.id} className={appearanceCss.item}>
                        <div className={appearanceCss.row}>
                          <div className={appearanceCss.rowText}>
                            <span className={appearanceCss.rowTitle}>{plugin.title || plugin.id}</span>
                            <span className={appearanceCss.rowHint}>{selected===undefined?'未限制，可使用全部模型':`仅允许 ${selected.length} 个模型`}</span>
                          </div>
                          <Switch
                            checked={selected!==undefined}
                            label={`限制 ${plugin.id} 可用模型`}
                            onChange={checked=>setModels(checked?draft.providers.filter(p=>p.api_type==='openai-completions').flatMap(p=>parseModels(p.models_text).map(model=>({provider:p.id,model}))):undefined)}
                          />
                        </div>
                        {selected!==undefined&&<div className={appearanceCss.checkGrid}>
                          {draft.providers.filter(p=>p.api_type==='openai-completions').flatMap(p=>parseModels(p.models_text).map(model=><label key={JSON.stringify([p.id,model])} className={modelsCss.checkRow}>
                            <input type="checkbox" checked={selected.some(r=>r.provider===p.id&&r.model===model)} onChange={e=>setModels(e.target.checked?[...selected,{provider:p.id,model}]:selected.filter(r=>r.provider!==p.id||r.model!==model))}/>
                            <span>{p.display_name||p.id} / {model}</span>
                          </label>))}
                        </div>}
                      </div>
                    })}
                  </div>
                  <p className={appearanceCss.footnote}>未限制时可使用全部模型；开启限制后仅允许勾选的模型，主对话、视觉与协作任务共同遵守。取消全部勾选会禁止该插件调用模型。</p>
                </div>
              </div>
            )}

            {activeTab === 'tools' && draft !== null && (
              <section className={modelsCss.section}>
                <div className={modelsCss.pageHead}>
                  <h2 className={modelsCss.title}>专用模型</h2>
                  <p className={modelsCss.intro}>视觉分析和协作子 Agent 可以单独选模型，复用目录中的提供商和能力配置。留空跟随主 Agent，任务参数单独保存。</p>
                </div>
                {(['visual_tool', 'delegate_agent'] as const).map(kind => {
                  const config = draft[kind]
                  const provider = draft.providers.find(p => p.id === (config.provider || draft.orchestrator_provider))
                  const capability = provider?.model_capabilities[config.model || draft.orchestrator_model]
                  const levels = capability?.reasoning_efforts.length ? capability.reasoning_efforts : ['none','low','medium','high','xhigh','max']
                  const update = (patch: Partial<ModelUseSettings>) => updateDraft({ [kind]: { ...config, ...patch } })
                  const customized = Boolean(config.provider || config.model || config.reasoning_effort || config.context_window)
                  return (
                    <div key={kind} className={appearanceCss.block}>
                      <div className={appearanceCss.labelRow}>
                        <div className={appearanceCss.label}>{kind === 'visual_tool' ? '视觉分析工具' : '协作子 Agent'}</div>
                        <button
                          type="button"
                          className={clsx(appearanceCss.link, appearanceCss.linkMuted)}
                          disabled={!customized}
                          onClick={() => updateDraft({ [kind]: {} })}
                        >
                          {customized ? '恢复为跟随主 Agent' : '跟随主 Agent'}
                        </button>
                      </div>
                      <div className={appearanceCss.group}>
                        <div className={appearanceCss.row}>
                          <span className={appearanceCss.rowTitle}>提供商</span>
                          <div className={appearanceCss.control}>
                            <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={config.provider || ''} onChange={e => update({ provider: e.target.value || null, model: null, reasoning_effort: null })}>
                              <option value="">跟随主 Agent</option>
                              {draft.providers.filter(p => p.api_type === 'openai-completions').map(p => <option key={p._draftId} value={p.id}>{p.display_name || p.id}</option>)}
                            </select>
                          </div>
                        </div>
                        <div className={appearanceCss.row}>
                          <span className={appearanceCss.rowTitle}>模型 ID</span>
                          <div className={appearanceCss.control}>
                            <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={config.model || ''} onChange={e => update({ model: e.target.value || null, reasoning_effort: null })}>
                              <option value="">跟随主 Agent</option>
                              {config.model && !parseModels(provider?.models_text ?? '').includes(config.model) && <option value={config.model} disabled>{config.model}（未在模型目录中，请先添加）</option>}
                              {(provider ? parseModels(provider.models_text) : []).map(m => <option key={m} value={m}>{m}</option>)}
                            </select>
                          </div>
                        </div>
                        <div className={appearanceCss.row}>
                          <span className={appearanceCss.rowTitle}>推理档位</span>
                          <div className={appearanceCss.control}>
                            <select className={clsx(modelsCss.input, modelsCss.selectInput)} value={config.reasoning_effort || ''} onChange={e => update({ reasoning_effort: e.target.value || null })}>
                              <option value="">跟随主 Agent</option>
                              {levels.map(level => <option key={level} value={level}>{({ none:'无', low:'轻度', medium:'中档', high:'高', xhigh:'更高', max:'最高' } as Record<string,string>)[level]}</option>)}
                            </select>
                          </div>
                        </div>
                        <div className={appearanceCss.row}>
                          <div className={appearanceCss.rowText}>
                            <span className={appearanceCss.rowTitle}>最大上下文</span>
                            <span className={appearanceCss.rowHint}>单位 tokens</span>
                          </div>
                          <div className={appearanceCss.control}>
                            <input className={modelsCss.input} type="number" min={32768} max={1050000} step={1} placeholder="留空跟随主 Agent" value={config.context_window ?? ''} onChange={e => update({ context_window: e.target.value === '' ? null : Number(e.target.value) })} />
                          </div>
                        </div>
                      </div>
                      <p className={appearanceCss.footnote}>{{
                        visual_tool: '分析已保存图片，结果通过普通工具返回。',
                        delegate_agent: '主 Agent 派出去并行干活的子 Agent 用这个模型。可以让主 Agent 用强模型负责拆分、审查和集成，子 Agent 用更省的模型执行。留空则和主 Agent 用同一个。',
                      }[kind]}</p>
                    </div>
                  )
                })}
              </section>
            )}

            {activeTab === 'plugins' && (
              <div className={pluginsCss.section}>
                <div className={clsx(modelsCss.modelListHead, modelsCss.pageHead)}>
                  <div className={modelsCss.cardHeadText}>
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
