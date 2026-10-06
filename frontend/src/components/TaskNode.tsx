import { memo } from 'react'
import { Handle, Position, type NodeProps, type Node } from '@xyflow/react'
import type { FlowNodeData } from '../model.ts'
import styles from './TaskNode.module.css'

const KIND_META: Record<string, { label: string; className: string; accentClass: string; icon: string }> = {
  research: {
    label: '探索',
    className: styles.kindResearch ?? '',
    accentClass: styles.accentResearch ?? '',
    icon: '🟣',
  },
  coding: {
    label: '实施',
    className: styles.kindCoding ?? '',
    accentClass: styles.accentCoding ?? '',
    icon: '🔵',
  },
  verify: {
    label: '验证',
    className: styles.kindVerify ?? '',
    accentClass: styles.accentVerify ?? '',
    icon: '🟢',
  },
  issue: {
    label: '问题',
    className: styles.kindIssue ?? '',
    accentClass: styles.accentIssue ?? '',
    icon: '🟡',
  },
  gate: {
    label: '审查',
    className: styles.kindGate ?? '',
    accentClass: styles.accentGate ?? '',
    icon: '🔴',
  },
  plan: {
    label: '规划',
    className: styles.kindPlan ?? '',
    accentClass: styles.accentPlan ?? '',
    icon: '⚪',
  },
}

function formatNodeId(id: string): string {
  const shortId = id.split(':').pop() || id
  if (shortId.length > 12) return `#${shortId.slice(0, 8)}…`
  return `#${shortId}`
}

function cleanDescription(desc?: string): string {
  if (!desc) return ''
  const trimmed = desc.trim()
  if (trimmed.startsWith('{') && trimmed.endsWith('}')) {
    try {
      const parsed = JSON.parse(trimmed) as Record<string, unknown>
      if (typeof parsed === 'object' && parsed !== null) {
        if (typeof parsed.stderr === 'string' && parsed.stderr.trim()) {
          const lines = parsed.stderr.trim().split(/\r?\n/).filter(Boolean)
          return lines.slice(-2).join(' ') || parsed.stderr.trim()
        }
        if (typeof parsed.error === 'string' && parsed.error.trim()) {
          return parsed.error.trim()
        }
        if (typeof parsed.reason === 'string' && parsed.reason.trim()) {
          return parsed.reason.trim()
        }
      }
    } catch {}
  }
  return trimmed
    .replace(/\\r\\n/g, ' ')
    .replace(/\\n/g, ' ')
    .replace(/\\"/g, '"')
    .replace(/\s+/g, ' ')
}

export const TaskNode = memo(function TaskNode({
  data,
  selected,
}: NodeProps<Node<FlowNodeData>>) {
  const meta = KIND_META[data.kind] || {
    label: data.kind || '步骤',
    className: styles.kindPlan ?? '',
    accentClass: styles.accentPlan ?? '',
    icon: '⚪',
  }
  const isRunning = data.status === 'running'
  const isCompleted = data.status === 'completed'
  const isSkipped = data.status === 'skipped'
  const isInterrupted = data.status === 'interrupted'
  const isFailed = data.status === 'failed'
  const isDeprecated = data.status === 'deprecated'

  const renderStatus = () => {
    if (isDeprecated) {
      return (
        <span
          className={styles.statusIcon}
          style={{ color: '#9ca3af', fontSize: '11px', fontWeight: 600 }}
          title={data.invalidatedByPlanRevision ? `已废弃（在 Plan Rev ${data.invalidatedByPlanRevision} 回溯时退出执行集合）` : '已废弃'}
        >
          已废弃
        </span>
      )
    }
    if (data.status === 'waiting_children') return <span className={styles.statusIcon} title="等待子问题，父任务尚未完成">⏳</span>
    if (data.status === 'paused') return <span className={styles.statusIcon} title="待继续">⏸</span>
    if (data.status === 'blocked') return <span className={styles.statusIcon} title="受阻，等待解决">⚠</span>
    if (isRunning) {
      return <div className={styles.spinner} title="进行中" />
    }
    if (isCompleted) {
      return (
        <span
          className={styles.statusIcon}
          style={{ color: 'var(--dsw-alias-state-success-primary, #22c55e)' }}
          title="已完成"
        >
          ✓
        </span>
      )
    }
    if (isSkipped) {
      return <span className={styles.statusIcon} title="已跳过">—</span>
    }
    if (isInterrupted) {
      return (
        <span
          className={styles.statusIcon}
          style={{ color: 'var(--dsw-alias-state-warn-primary, #f59e0b)' }}
          title="已中断 / 暂停"
        >
          ⏸
        </span>
      )
    }
    if (isFailed) {
      return (
        <span
          className={styles.statusIcon}
          style={{ color: 'var(--dsw-alias-state-error-primary, #ec1313)' }}
          title="已失败"
        >
          ✕
        </span>
      )
    }
    return (
      <span
        className={styles.statusIcon}
        style={{ color: 'var(--dsw-alias-state-idle-primary, #d1d5db)' }}
        title="待处理"
      >
        ○
      </span>
    )
  }

  const fileCount = data.files ? data.files.length : 0
  const toolCount = data.tools ? data.tools.length : 0
  const consultCount = data.consults ? data.consults.length : 0
  const displayDesc = cleanDescription(data.objective ?? data.description)

  return (
    <div
      className={`${styles.nodeWrapper} ${isDeprecated ? styles.deprecated : meta.accentClass} ${
        selected ? styles.selected : ''
      } ${isRunning ? styles.running : ''}`}
      data-node-id={data.id}
    >
      {/* 4 Handles for clean directional DAG routing */}
      <Handle
        type="target"
        position={Position.Top}
        id="target-top"
        className={styles.handleBase}
      />
      <Handle
        type="source"
        position={Position.Bottom}
        id="source-bottom"
        className={styles.handleBase}
      />
      <Handle
        type="target"
        position={Position.Left}
        id="target-left"
        className={styles.handleBase}
      />
      <Handle
        type="source"
        position={Position.Right}
        id="source-right"
        className={styles.handleBase}
      />

      <div className={styles.header}>
        <div className={styles.badgeGroup}>
          <span className={`${styles.kindBadge} ${meta.className}`}>
            <span>{meta.icon}</span>
            <span>{meta.label}</span>
          </span>
          <span className={styles.nodeIdTag} title={data.id}>
            {formatNodeId(data.id)}
          </span>
        </div>
        <div className={styles.statusIcon}>{renderStatus()}</div>
      </div>

      <div className={styles.title} title={data.title}>
        {data.title}
      </div>

      {displayDesc && (
        <div className={styles.description} title={data.description ?? displayDesc}>
          {displayDesc}
        </div>
      )}

      {(toolCount > 0 || fileCount > 0 || consultCount > 0) && (
        <div className={styles.footer}>
          {toolCount > 0 && (
            <span className={styles.toolPill} title={`调用了 ${toolCount} 个工具`}>
              ⚡ {toolCount} 工具
            </span>
          )}
          {fileCount > 0 && data.files[0] && (
            <span className={styles.filePill} title={data.files.join(', ')}>
              📄 {data.files[0].split(/[/\\]/).pop()}
              {fileCount > 1 ? ` +${fileCount - 1}` : ''}
            </span>
          )}
          {consultCount > 0 && (
            <span
              className={styles.consultPill}
              title={`与观察者交流了 ${consultCount} 次`}
            >
              👁 观察者 ({consultCount})
            </span>
          )}
        </div>
      )}
    </div>
  )
})
