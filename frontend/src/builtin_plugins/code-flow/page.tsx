import { useEffect, useMemo, useRef, useState } from 'react'
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  Handle,
  Position,
  type NodeProps,
  type Node,
  MarkerType,
} from '@xyflow/react'
import '@xyflow/react/dist/style.css'
import clsx from 'clsx'

import { usePluginTools } from '../../plugins/react.tsx'
import type { PluginPageProps } from '../../plugins/protocol.ts'
import { layoutCodeFlow, type CodeGraph, type CodeNode, type CodeFeature } from './graph.ts'
import {
  CodeBlock,
  languageForPath,
  writeClipboard,
  IconBranchOutlineRegular,
  IconSearchOutlineRegular,
  IconRefreshOutlineRegular,
  IconCopyOutlineRegular,
  IconDownloadOutlineRegular,
  IconCloseOutlineRegular,
  IconFolderOpenRegular,
  IconFolderCloseRegular,
  IconCheckOutlineRegular,
  IconChevronRightOutlineRegular,
  IconCodeOutlineRegular,
  IconInspectOutlineRegular,
  IconLoadingOutlineRegular,
} from '../../dsh/ui-primitives/index.ts'
import css from './style.module.css'

const ROLES: Record<string, { label: string; accentClass?: string | undefined; roleClass?: string | undefined }> = {
  entry: { label: '入口', accentClass: css.accentEntry, roleClass: css.role_entry },
  unit: { label: '独立职责', accentClass: css.accentUnit, roleClass: css.role_unit },
  shared: { label: '公共能力', accentClass: css.accentShared, roleClass: css.role_shared },
}

const STATUS_LABELS: Record<string, string> = {
  current: '源码已定位',
  missing: '源码缺失',
  unresolved: '定位不明确',
}

function MethodNode({ data, selected }: NodeProps<Node<CodeNode & Record<string, unknown>>>) {
  const roleMeta = ROLES[data.role] ?? { label: data.role, accentClass: '', roleClass: '' }
  const statusLabel = STATUS_LABELS[data.status] ?? data.status

  return (
    <div className={clsx(css.node, roleMeta.accentClass, selected && css.selected)}>
      <Handle type="target" position={Position.Top} className={css.handle} />

      <div className={css.nodeHeader}>
        <div className={css.roleGroup}>
          <span className={clsx(css.roleBadge, roleMeta.roleClass)}>{roleMeta.label}</span>
          <span className={css.statusDot} data-status={data.status} title={statusLabel} />
        </div>
        {data.test_entries && data.test_entries.length > 0 && (
          <span className={css.testPill} title={`已关联 ${data.test_entries.length} 个测试入口`}>
            {data.test_entries.length} 测试
          </span>
        )}
      </div>

      <div className={css.nodeLabel} title={data.label}>
        {data.label}
      </div>

      <div className={css.nodeCodeName}>
        <code title={data.name}>{data.name}</code>
      </div>

      {data.responsibility && (
        <div className={css.nodeSummary} title={data.responsibility}>
          {data.responsibility}
        </div>
      )}

      <Handle type="source" position={Position.Bottom} className={css.handle} />
    </div>
  )
}

const nodeTypes = { method: MethodNode }

export default function CodeFlowView({ workspaceRoot, owner }: PluginPageProps) {
  const api = usePluginTools(owner)
  const [root, setRoot] = useState(() => localStorage.getItem('code_flow_workspace') ?? workspaceRoot)
  const [data, setData] = useState<CodeGraph>()
  const [selected, setSelected] = useState<string>()
  const [detail, setDetail] = useState<CodeGraph>()
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const [detailLoading, setDetailLoading] = useState(false)
  const [detailError, setDetailError] = useState('')
  const [copied, setCopied] = useState(false)
  const [methodNameCopied, setMethodNameCopied] = useState(false)
  const [featureSearch, setFeatureSearch] = useState('')

  const generation = useRef(0)
  const detailGeneration = useRef(0)

  useEffect(() => {
    void load()
    return () => {
      generation.current++
      detailGeneration.current++
    }
  }, [])

  // Close drawer on Escape
  useEffect(() => {
    if (!selected) return
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setSelected(undefined)
        setDetail(undefined)
      }
    }
    window.addEventListener('keydown', handleKeyDown)
    return () => window.removeEventListener('keydown', handleKeyDown)
  }, [selected])

  async function load(featureId?: string) {
    const token = ++generation.current
    detailGeneration.current++
    setSelected(undefined)
    setDetail(undefined)
    setDetailError('')
    setError('')
    setBusy(true)
    setCopied(false)

    try {
      const next = await api.call<CodeGraph>('read_code_flow', {
        workspace_root: root,
        ...(featureId ? { feature_id: featureId } : {}),
      })
      if (token !== generation.current) return
      setData(next)
      localStorage.setItem('code_flow_workspace', next.workspace_root)
    } catch (reason) {
      if (token === generation.current) {
        setError(String(reason))
        setData(undefined)
      }
    } finally {
      if (token === generation.current) setBusy(false)
    }
  }

  async function inspect(id: string) {
    const featureId = data?.feature?.id
    if (!featureId || !data) return
    const token = ++detailGeneration.current
    setSelected(id)
    setDetail(undefined)
    setDetailError('')
    setDetailLoading(true)

    try {
      const next = await api.call<CodeGraph>('read_code_flow', {
        workspace_root: data.workspace_root,
        feature_id: featureId,
        node_id: id,
      })
      if (token === detailGeneration.current) {
        setDetail(next)
      }
    } catch (reason) {
      if (token === detailGeneration.current) {
        setDetailError(String(reason))
      }
    } finally {
      if (token === detailGeneration.current) {
        setDetailLoading(false)
      }
    }
  }

  const graph = useMemo(() => {
    const ids = (data?.nodes ?? []).map(node => node.id)
    const positions = layoutCodeFlow(ids, data?.edges ?? [])

    return {
      nodes: (data?.nodes ?? []).map(node => ({
        id: node.id,
        type: 'method',
        position: positions.get(node.id) ?? { x: 50, y: 50 },
        data: { ...node },
        selected: node.id === selected,
      })),
      edges: (data?.edges ?? []).map(edge => {
        const isConnected = selected && (edge.source === selected || edge.target === selected)
        return {
          ...edge,
          type: 'smoothstep',
          markerEnd: {
            type: MarkerType.ArrowClosed,
            color: isConnected ? '#2563eb' : '#94a3b8',
            width: 14,
            height: 14,
          },
          style: {
            stroke: isConnected ? '#2563eb' : '#94a3b8',
            strokeWidth: isConnected ? 2.5 : 1.5,
            opacity: selected ? (isConnected ? 1 : 0.35) : 0.85,
          },
          animated: Boolean(isConnected),
        }
      }),
    }
  }, [data, selected])

  const current = data?.nodes?.find(node => node.id === selected)
  const children = (data?.features ?? []).filter(
    feature => feature.parent === (data?.feature?.id ?? null)
  )

  const filteredFeatures = useMemo(() => {
    if (!data?.features) return []
    if (!featureSearch.trim()) return data.features
    const query = featureSearch.trim().toLowerCase()
    return data.features.filter(
      f => f.title.toLowerCase().includes(query) || f.summary.toLowerCase().includes(query)
    )
  }, [data?.features, featureSearch])

  // Breadcrumbs path
  const breadcrumbs = useMemo(() => {
    if (!data?.feature || !data?.features) return []
    const path: CodeFeature[] = []
    let currentId: string | null = data.feature.id
    while (currentId) {
      const found = data.features.find(f => f.id === currentId)
      if (found) {
        path.unshift(found)
        currentId = found.parent
      } else {
        break
      }
    }
    return path
  }, [data?.feature, data?.features])

  async function copyGraph() {
    if (!data) return
    try {
      await writeClipboard(JSON.stringify(data, null, 2))
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } catch (reason) {
      setError(`复制失败：${String(reason)}`)
    }
  }

  function downloadGraph() {
    if (!data) return
    const url = URL.createObjectURL(
      new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' })
    )
    const link = document.createElement('a')
    link.href = url
    link.download = `code-flow-${data.feature?.id ?? 'catalog'}.json`
    link.click()
    URL.revokeObjectURL(url)
  }

  async function copyMethodName(name: string) {
    await writeClipboard(name)
    setMethodNameCopied(true)
    setTimeout(() => setMethodNameCopied(false), 1500)
  }

  return (
    <section className={css.container} aria-label="代码 Flow 架构调用图">
      {/* Sleek Top App Bar */}
      <header className={css.topBar}>
        <div className={css.brandGroup}>
          <div className={css.brandIcon}>
            <IconBranchOutlineRegular size={16} />
          </div>
          <div className={css.brandTitle}>
            <span>代码 Flow</span>
            <span className={css.brandBadge}>架构静态调用图</span>
          </div>
        </div>

        <form
          className={css.workspaceForm}
          onSubmit={event => {
            event.preventDefault()
            void load()
          }}
        >
          <div className={css.workspaceInputWrap}>
            <span className={css.workspaceIcon}>
              <IconFolderOpenRegular size={14} />
            </span>
            <input
              className={css.workspaceInput}
              aria-label="代码工作区路径"
              placeholder="代码工作区根目录"
              value={root}
              onChange={event => setRoot(event.target.value)}
            />
          </div>
          <button
            type="submit"
            className={clsx(css.pillBtn, css.pillBtnPrimary)}
            disabled={busy || !root.trim()}
          >
            读取功能
          </button>
        </form>

        <div className={css.topBarActions}>
          {data?.scope === 'node_neighborhood' && (
            <div className={css.scopeBadge}>
              <IconInspectOutlineRegular size={13} />
              <span>局部依赖聚焦</span>
              <button
                type="button"
                className={css.breadcrumbBtn}
                onClick={() => void load(data.feature?.id)}
                style={{ marginLeft: 4 }}
              >
                返回完整功能
              </button>
            </div>
          )}

          <button
            type="button"
            className={css.pillBtn}
            disabled={busy || !data}
            onClick={() => void load(data?.feature?.id)}
            title="重新扫描当前功能并刷新调用流"
          >
            <IconRefreshOutlineRegular size={13} />
            <span>刷新源码</span>
          </button>

          <button
            type="button"
            className={clsx(css.pillBtn, copied && css.copiedBtn)}
            disabled={!data}
            onClick={() => void copyGraph()}
            title="复制完整调用图 JSON 供 AI 分析"
          >
            {copied ? <IconCheckOutlineRegular size={13} /> : <IconCopyOutlineRegular size={13} />}
            <span>{copied ? '已复制 JSON' : '复制给 AI'}</span>
          </button>

          <button
            type="button"
            className={css.pillBtn}
            disabled={!data}
            onClick={downloadGraph}
            title="下载当前功能调用图 JSON"
          >
            <IconDownloadOutlineRegular size={13} />
            <span>导出 JSON</span>
          </button>
        </div>
      </header>

      {error && (
        <div className={css.errorBanner} role="alert">
          <span>{error}</span>
          <button
            type="button"
            className={css.closeBtn}
            onClick={() => setError('')}
            title="关闭提示"
          >
            <IconCloseOutlineRegular size={14} />
          </button>
        </div>
      )}

      <div className={css.body}>
        {/* Left Features Sidebar */}
        <aside className={css.sidebar} aria-label="功能分级导航">
          <div className={css.sidebarHeader}>
            <span className={css.sidebarTitle}>功能目录</span>
            {data?.features && (
              <span className={css.navItemBadge}>{data.features.length}</span>
            )}
          </div>

          <div className={css.sidebarSearchWrap}>
            <span className={css.sidebarSearchIcon}>
              <IconSearchOutlineRegular size={12} />
            </span>
            <input
              className={css.sidebarSearchInput}
              placeholder="搜索功能模块..."
              value={featureSearch}
              onChange={e => setFeatureSearch(e.target.value)}
            />
          </div>

          <nav className={css.sidebarNav}>
            <button
              type="button"
              className={clsx(css.navItem, !data?.feature && css.navItemActive)}
              onClick={() => void load()}
            >
              <div className={css.navItemContent}>
                <span className={css.navItemIcon}>
                  <IconFolderCloseRegular size={14} />
                </span>
                <span className={css.navItemText}>全部功能概览</span>
              </div>
            </button>

            {filteredFeatures.map(feature => {
              const isChild = Boolean(feature.parent)
              const isActive = feature.id === data?.feature?.id

              return (
                <button
                  key={feature.id}
                  type="button"
                  className={clsx(css.navItem, isActive && css.navItemActive)}
                  style={{ paddingLeft: isChild ? 22 : 8 }}
                  onClick={() => void load(feature.id)}
                  title={`${feature.title}: ${feature.summary}`}
                >
                  <div className={css.navItemContent}>
                    <span className={css.navItemIcon}>
                      {feature.node_count ? (
                        <IconCodeOutlineRegular size={13} />
                      ) : (
                        <IconFolderCloseRegular size={13} />
                      )}
                    </span>
                    <span className={css.navItemText}>{feature.title}</span>
                  </div>
                  <span className={css.navItemBadge}>
                    {feature.node_count ? `${feature.node_count} 节点` : '分组'}
                  </span>
                </button>
              )
            })}
          </nav>

          <div className={css.sidebarLegend}>
            <div className={css.legendTitle}>节点类型图例</div>
            <div className={css.legendRow}>
              <span className={css.legendItem}>
                <span className={css.dotEntry} /> 入口
              </span>
              <span className={css.legendItem}>
                <span className={css.dotUnit} /> 独立职责
              </span>
              <span className={css.legendItem}>
                <span className={css.dotShared} /> 公共能力
              </span>
            </div>
            <div className={css.legendTip}>
              连线代表源码静态调用证据；宏与动态派发未解析。
            </div>
          </div>
        </aside>

        {/* Main Canvas & Context Area */}
        <main className={css.main}>
          <div className={css.heading}>
            {breadcrumbs.length > 0 && (
              <nav className={css.breadcrumbs} aria-label="路径导航">
                <button
                  type="button"
                  className={css.breadcrumbBtn}
                  onClick={() => void load()}
                >
                  全部功能
                </button>
                {breadcrumbs.map((crumb, idx) => (
                  <span key={crumb.id} style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
                    <IconChevronRightOutlineRegular size={10} />
                    {idx === breadcrumbs.length - 1 ? (
                      <span>{crumb.title}</span>
                    ) : (
                      <button
                        type="button"
                        className={css.breadcrumbBtn}
                        onClick={() => void load(crumb.id)}
                      >
                        {crumb.title}
                      </button>
                    )}
                  </span>
                ))}
              </nav>
            )}

            <div className={css.headingRow}>
              <h2 className={css.headingTitle}>
                {data?.feature?.title ?? '全部功能概览'}
              </h2>
              {data?.feature && (
                <div className={css.coverageChips}>
                  <span className={css.coverageChip}>
                    {data.nodes?.length ?? 0} 个方法
                  </span>
                  <span className={css.coverageChip}>
                    {data.edges?.length ?? 0} 处调用
                  </span>
                  <span className={css.coverageChip}>
                    {data.boundary_calls?.length ?? 0} 处边界
                  </span>
                </div>
              )}
            </div>

            <p className={css.headingSummary}>
              {data?.feature?.summary ??
                '逐级查看架构功能与方法调用链，点击节点可检查实现源码、依赖边界与测试入口。'}
            </p>
          </div>

          <div className={css.canvasViewport}>
            {busy ? (
              <div className={css.loadingState}>
                <IconLoadingOutlineRegular size={24} />
                <span>正在分析源码并构建调用流图…</span>
              </div>
            ) : graph.nodes.length > 0 ? (
              <ReactFlow
                key={`${data?.feature?.id}:${data?.selected_node?.id ?? 'all'}`}
                nodes={graph.nodes}
                edges={graph.edges}
                nodeTypes={nodeTypes}
                fitView
                fitViewOptions={{ padding: 0.2 }}
                minZoom={0.15}
                maxZoom={1.6}
                nodesDraggable={false}
                nodesConnectable={false}
                onNodeClick={(_, node) => void inspect(node.id)}
                onPaneClick={() => {
                  setSelected(undefined)
                  setDetail(undefined)
                }}
              >
                <Background color="#cbd5e1" gap={22} size={1} />
                <Controls showInteractive={false} />
                <MiniMap
                  nodeStrokeWidth={2}
                  zoomable
                  pannable
                  className={css.miniMap}
                  nodeColor={node => {
                    const d = node.data as unknown as CodeNode | undefined
                    if (d?.role === 'entry') return '#10b981'
                    if (d?.role === 'unit') return '#3b82f6'
                    if (d?.role === 'shared') return '#8b5cf6'
                    return '#94a3b8'
                  }}
                />
              </ReactFlow>
            ) : (
              <div className={css.cardsGrid}>
                {children.map(feature => (
                  <button
                    key={feature.id}
                    type="button"
                    className={css.featureCard}
                    onClick={() => void load(feature.id)}
                  >
                    <div className={css.cardHeader}>
                      <span className={css.cardTitle}>
                        <IconFolderCloseRegular size={15} />
                        <span>{feature.title}</span>
                      </span>
                      <span className={css.navItemBadge}>
                        {feature.node_count ? `${feature.node_count} 个方法` : '功能分组'}
                      </span>
                    </div>
                    <p className={css.cardSummary}>{feature.summary}</p>
                    <div className={css.cardFooter}>
                      <span>进入功能流图</span>
                      <IconChevronRightOutlineRegular size={12} />
                    </div>
                  </button>
                ))}
                {children.length === 0 && (
                  <div className={css.emptyCards}>
                    <IconBranchOutlineRegular size={32} />
                    <p style={{ marginTop: 12 }}>
                      当前分类暂无子功能或方法节点。请从左侧菜单选择具体的功能模块。
                    </p>
                  </div>
                )}
              </div>
            )}
          </div>

          {data?.feature && (
            <footer className={css.coverageFooter}>
              <div className={css.coverageChips}>
                <span>
                  当前功能指标：{data.nodes?.length ?? 0} 个节点 · {data.edges?.length ?? 0} 处显式调用 · {data.boundary_calls?.length ?? 0} 处边界未解析
                </span>
              </div>
              <span>Rust 静态符号调用图 · 宏与动态派发未解析</span>
            </footer>
          )}
        </main>

        {/* Selected Node Details Drawer */}
        {current && (
          <aside className={css.drawer} aria-label="方法详情抽屉">
            <header className={css.drawerHeader}>
              <div className={css.drawerTitleGroup}>
                <div className={css.drawerMetaRow}>
                  <span
                    className={clsx(
                      css.roleBadge,
                      ROLES[current.role]?.roleClass ?? css.role_unit
                    )}
                  >
                    {ROLES[current.role]?.label ?? current.role}
                  </span>
                  <span
                    className={css.statusDot}
                    data-status={current.status}
                    title={STATUS_LABELS[current.status] ?? current.status}
                  />
                  <span style={{ fontSize: 11, color: 'var(--codex-text-muted)' }}>
                    {STATUS_LABELS[current.status] ?? current.status}
                  </span>
                </div>
                <h3 className={css.drawerTitle} title={current.label}>
                  {current.label}
                </h3>
                <div className={css.drawerCode}>
                  <code>{current.name}</code>
                  <button
                    type="button"
                    className={css.iconCopyBtn}
                    onClick={() => void copyMethodName(current.name)}
                    title={methodNameCopied ? '已复制方法名' : '复制方法名'}
                  >
                    {methodNameCopied ? (
                      <IconCheckOutlineRegular size={12} />
                    ) : (
                      <IconCopyOutlineRegular size={12} />
                    )}
                  </button>
                </div>
              </div>
              <button
                type="button"
                className={css.closeBtn}
                onClick={() => {
                  detailGeneration.current++
                  setSelected(undefined)
                  setDetail(undefined)
                }}
                title="关闭详情 (Esc)"
              >
                <IconCloseOutlineRegular size={15} />
              </button>
            </header>

            <div className={css.drawerActions}>
              <button
                type="button"
                className={clsx(css.pillBtn, css.pillBtnPrimary)}
                disabled={!detail}
                onClick={() => {
                  if (detail) setData(detail)
                }}
              >
                <IconInspectOutlineRegular size={13} />
                <span>聚焦该节点及直接依赖</span>
              </button>

              {data?.scope === 'node_neighborhood' && (
                <button
                  type="button"
                  className={css.pillBtn}
                  onClick={() => void load(data.feature?.id)}
                >
                  返回完整功能
                </button>
              )}
            </div>

            <div className={css.drawerContent}>
              {/* Responsibility Overview */}
              <div className={css.section}>
                <div className={css.callout}>
                  <strong>职责描述：</strong>
                  {current.responsibility}
                </div>

                <div className={css.propertyCard}>
                  <div className={css.kvRow}>
                    <span className={css.kvKey}>输入参数</span>
                    <span className={css.kvValue}>{current.inputs || '无'}</span>
                  </div>
                  <div className={css.kvRow}>
                    <span className={css.kvKey}>输出返回值</span>
                    <span className={css.kvValue}>{current.outputs || '无'}</span>
                  </div>
                  <div className={css.kvRow}>
                    <span className={css.kvKey}>源码位置</span>
                    <span className={css.kvValue}>
                      <span className={css.codePill}>
                        {current.source
                          ? `${current.source.file}:${current.source.line}`
                          : current.file}
                      </span>
                    </span>
                  </div>
                  <div className={css.kvRow}>
                    <span className={css.kvKey}>版本签名</span>
                    <span className={css.kvValue}>
                      <span className={css.codePill}>
                        {current.source?.code_hash ?? current.status}
                      </span>
                    </span>
                  </div>
                </div>
              </div>

              {/* Source Code */}
              <details className={css.accordion} open>
                <summary className={css.accordionSummary}>
                  <span>当前方法源码</span>
                  <span style={{ fontSize: 11, color: 'var(--codex-text-muted)' }}>
                    {current.source
                      ? `L${current.source.line}-L${current.source.end_line}`
                      : '源码'}
                  </span>
                </summary>
                <div className={css.accordionBody}>
                  {detailLoading ? (
                    <div className={css.emptyHint}>正在读取源码文件…</div>
                  ) : detailError ? (
                    <div className={css.errorBanner} style={{ borderRadius: 6 }}>
                      {detailError}
                    </div>
                  ) : detail?.source_content ? (
                    <CodeBlock
                      code={detail.source_content}
                      lang={
                        languageForPath(current.source?.file ?? current.file) ?? 'rust'
                      }
                      lineNumbers={true}
                      copyLabel="复制代码"
                      copiedLabel="已复制"
                    />
                  ) : (
                    <div className={css.emptyHint}>未检索到源码内容</div>
                  )}
                </div>
              </details>

              {/* Internal Calls */}
              <details className={css.accordion} open>
                <summary className={css.accordionSummary}>
                  <span>功能内调用与被调</span>
                  <span className={css.navItemBadge}>
                    {(data?.edges ?? []).filter(
                      edge => edge.source === current.id || edge.target === current.id
                    ).length}
                  </span>
                </summary>
                <div className={css.accordionBody}>
                  {(data?.edges ?? [])
                    .filter(edge => edge.source === current.id || edge.target === current.id)
                    .map(edge => {
                      const isOutgoing = edge.source === current.id
                      const otherId = isOutgoing ? edge.target : edge.source
                      const otherNode = data?.nodes?.find(n => n.id === otherId)

                      return (
                        <div key={edge.id} className={css.callItem}>
                          <div className={css.callHeader}>
                            <div className={css.callFlowLine}>
                              <span>{isOutgoing ? '调用 ➔' : '被调 ⬅'}</span>
                              <button
                                type="button"
                                className={css.callTargetBtn}
                                onClick={() => void inspect(otherId)}
                                title="点击聚焦查看该节点"
                              >
                                {otherNode?.label ?? otherId}
                              </button>
                            </div>
                            <span className={css.callEvidenceFile}>
                              {edge.evidence.file}:{edge.evidence.line}
                            </span>
                          </div>
                          {edge.evidence.snippet && (
                            <pre className={css.callSnippet}>{edge.evidence.snippet}</pre>
                          )}
                        </div>
                      )
                    })}
                  {(data?.edges ?? []).filter(
                    edge => edge.source === current.id || edge.target === current.id
                  ).length === 0 && (
                    <div className={css.emptyHint}>本功能内无直接上下游连线</div>
                  )}
                </div>
              </details>

              {/* Boundary Calls */}
              <details className={css.accordion}>
                <summary className={css.accordionSummary}>
                  <span>边界与未解析调用</span>
                  <span className={css.navItemBadge}>
                    {(data?.boundary_calls ?? []).filter(
                      call => call.source === current.id
                    ).length}
                  </span>
                </summary>
                <div className={css.accordionBody}>
                  {(data?.boundary_calls ?? [])
                    .filter(call => call.source === current.id)
                    .map((call, i) => (
                      <div key={i} className={css.callItem}>
                        <div className={css.callHeader}>
                          <span style={{ fontWeight: 600 }}>
                            <code>
                              {call.qualifier ? `${call.qualifier}::` : ''}
                              {call.target_text}
                            </code>
                          </span>
                          <span className={css.callEvidenceFile}>
                            {call.evidence.file}:{call.evidence.line}
                          </span>
                        </div>
                        <div style={{ fontSize: 11, color: 'var(--codex-text-muted)' }}>
                          状态：{call.resolution}
                        </div>
                        {call.evidence.snippet && (
                          <pre className={css.callSnippet}>{call.evidence.snippet}</pre>
                        )}
                      </div>
                    ))}
                  {(data?.boundary_calls ?? []).filter(
                    call => call.source === current.id
                  ).length === 0 && (
                    <div className={css.emptyHint}>无边界或跨模块调用记录</div>
                  )}
                </div>
              </details>

              {/* External Callers */}
              <details className={css.accordion}>
                <summary className={css.accordionSummary}>
                  <span>功能外上游调用者</span>
                  <span className={css.navItemBadge}>
                    {(data?.external_callers ?? []).filter(
                      call => call.target === current.id
                    ).length}
                  </span>
                </summary>
                <div className={css.accordionBody}>
                  {(data?.external_callers ?? [])
                    .filter(call => call.target === current.id)
                    .map((call, i) => (
                      <div key={i} className={css.callItem}>
                        <div className={css.callHeader}>
                          <span style={{ fontWeight: 600 }}>
                            <code>{call.name}</code>
                          </span>
                          <span className={css.callEvidenceFile}>
                            {call.file}:{call.line}
                          </span>
                        </div>
                      </div>
                    ))}
                  {(data?.external_callers ?? []).filter(
                    call => call.target === current.id
                  ).length === 0 && (
                    <div className={css.emptyHint}>无外部调用者记录</div>
                  )}
                </div>
              </details>

              {/* Node Test Entries */}
              <details className={css.accordion}>
                <summary className={css.accordionSummary}>
                  <span>关联测试入口</span>
                  <span className={css.navItemBadge}>
                    {current.test_entries?.length ?? 0}
                  </span>
                </summary>
                <div className={css.accordionBody}>
                  {current.test_entries && current.test_entries.length > 0 ? (
                    current.test_entries.map(test => {
                      const cmd = test.argv.join(' ')
                      return (
                        <div key={test.filter} className={css.testItem}>
                          <div className={css.testStatusRow}>
                            <span
                              style={{
                                color:
                                  test.status === 'missing'
                                    ? 'var(--codex-amber-circle, #d97706)'
                                    : 'var(--codex-diff-add, #16a34a)',
                                fontWeight: 500,
                              }}
                            >
                              {test.status === 'missing'
                                ? '测试源码未找到'
                                : '已关联独立测试'}
                            </span>
                          </div>
                          <div className={css.commandBox}>
                            <code>{cmd}</code>
                            <button
                              type="button"
                              className={css.iconCopyBtn}
                              onClick={() => void copyMethodName(cmd)}
                              title="复制测试运行指令"
                            >
                              <IconCopyOutlineRegular size={12} />
                            </button>
                          </div>
                        </div>
                      )
                    })
                  ) : (
                    <div className={css.emptyHint}>尚未声明独立关联测试</div>
                  )}
                </div>
              </details>
            </div>
          </aside>
        )}
      </div>
    </section>
  )
}
