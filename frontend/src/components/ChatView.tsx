import { useEffect, useMemo, useState } from 'react'
import {
  IconChevronDownOutlineRegular,
  IconChevronRightOutlineRegular,
  StateDot,
  TextShimmer,
  projectUserText,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { useAgentApi } from '../cordis/react.tsx'
import type { AgentEvent, Task, ChangesSummary } from '../api.ts'
import { changeSummaries } from '../changes.ts'
import { ChangesCard } from './ChangesCard.tsx'
import chatCss from '../dsh/chat/ChatView.module.css'
import messageCss from '../dsh/chat/MessageItem.module.css'
import { markdownLabels } from '../dsh/markdown-labels.ts'
import { t } from '../i18n.ts'
import { awaitingModel, chatItems, formatDuration, type ChatItem, type TurnEndReason } from '../model.ts'
import { useTaskDuration } from '../useTaskDuration.ts'
import { AssistantMessage } from './AssistantMessage.tsx'
import css from './ChatView.module.css'
import { ToolRow } from './ToolRow.tsx'
import { SmoothMarkdown } from './SmoothMarkdown.tsx'

function TurnEnd({ reason, durationMs }: { reason: TurnEndReason; durationMs?: number | undefined }) {
  const durationText = durationMs !== undefined ? formatDuration(durationMs) : ''
  if (reason.kind === 'completed') {
    return null
  }
  if (reason.kind === 'aborted') {
    return (
      <div className={css.turnNote}>
        {t('app.aborted')}
        {durationText && <span className={css.turnDurationSpan}> · 耗时 {durationText}</span>}
      </div>
    )
  }
  return (
    <div className={messageCss.turnErrorRow} role="status">
      <StateDot state="error" className={messageCss.turnErrorDot} />
      <div className={messageCss.turnErrorCopy}>
        <span className={messageCss.turnErrorTitle}>
          {t('message.turnError')}
          {durationText && <span className={css.turnDurationSpan}> (耗时 {durationText})</span>}
        </span>
        <span className={messageCss.turnErrorMessage}>{reason.message}</span>
      </div>
      {reason.code !== undefined && <code className={messageCss.turnErrorCode}>{reason.code}</code>}
    </div>
  )
}

function Item({
  item,
  turnClosed,
  slashNames,
  onOpenTask,
}: {
  item: ChatItem
  turnClosed: boolean
  slashNames: readonly string[]
  onOpenTask: (taskId: string) => void
}) {
  switch (item.kind) {
    case 'turn':
      return null
    case 'user':
      return (
        <div className={messageCss.userRow}>
          <div className={messageCss.userStack}>
            <div className={messageCss.bubble}>{projectUserText(item.text, [], slashNames, 'skill')}</div>
          </div>
        </div>
      )
    case 'assistant':
      return <AssistantMessage text={item.text} reasoning={item.reasoning} streaming={item.streaming} />
    case 'progress':
      return (
        <div className={css.progressReport}>
          <strong>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" style={{ flex: 'none', color: 'var(--codex-amber-circle, #d97706)' }}>
              <polyline points="22 12 18 12 15 21 9 3 6 12 2 12" />
            </svg>
            当前目的：{item.purpose || '未填写'}
          </strong>
          {item.knownConditions.length > 0 && <span>已知：{item.knownConditions.join('；')}</span>}
          {item.nextAction && <span>接下来：{item.nextAction}</span>}
        </div>
      )
    case 'observer':
      return (
        <div className={css.observerAdvice}>
          <strong>
            <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" style={{ flex: 'none', color: 'var(--codex-blue-primary, #007aff)' }}>
              <path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7Z" />
              <circle cx="12" cy="12" r="3" />
            </svg>
            Observer 建议
          </strong>
          {item.summary && <span>{item.summary}</span>}
          <ul>{item.suggestions.map((suggestion, index) => <li key={index}>{suggestion}</li>)}</ul>
        </div>
      )
    case 'tool':
      return (
        <ToolRow
          name={item.name}
          rawArguments={item.arguments}
          result={item.result}
          stopped={turnClosed}
          onOpenChild={onOpenTask}
        />
      )
    case 'subagent':
      return (
        <button type="button" className={css.subagent} onClick={() => { onOpenTask(item.childTaskId) }}>
          <span className={css.subagentTitle}>{t('app.subagentStarted')}</span>
          <span className={css.subagentPrompt}>{item.prompt}</span>
          <span className={css.subagentLink}>{t('app.viewSubagent')}</span>
        </button>
      )
    case 'turn-end':
      return <TurnEnd reason={item.reason} durationMs={item.durationMs} />
  }
}

interface AssistantTurnData {
  turn: number
  key: string
  tools: ChatItem[]
  assistant?: (ChatItem & { kind: 'assistant' }) | undefined
  turnEnd?: (ChatItem & { kind: 'turn-end' }) | undefined
  closed: boolean
  durationMs?: number | undefined
}

function AssistantTurnView({
  turn,
  taskId,
  changes,
  trackingError,
  onChangesUpdated,
  running,
  liveDurationMs,
  slashNames,
  onOpenTask,
}: {
  turn: AssistantTurnData
  taskId: string
  changes: ChangesSummary | undefined
  trackingError: string | undefined
  onChangesUpdated: (() => void) | undefined
  running: boolean
  liveDurationMs?: number | undefined
  slashNames: readonly string[]
  onOpenTask: (taskId: string) => void
}) {
  const isTurnRunning = !turn.closed && running
  const [userToggled, setUserToggled] = useState<boolean | null>(null)
  const open = userToggled !== null ? userToggled : isTurnRunning

  const labels = useMemo(() => markdownLabels(t), [])


  const hasTools = turn.tools.length > 0
  const hasReasoning = Boolean(turn.assistant?.reasoning && turn.assistant.reasoning.trim() !== '')
  const duration = turn.durationMs ?? (isTurnRunning ? liveDurationMs : undefined)

  const headerLabel = useMemo(() => {
    if (isTurnRunning) {
      return duration !== undefined
        ? `思考与执行中 · 已用时 ${formatDuration(duration)}`
        : '思考与执行中…'
    }
    if (duration !== undefined) {
      return `用时 ${formatDuration(duration)}`
    }
    return '执行过程'
  }, [duration, isTurnRunning])

  const conclusionText = turn.assistant?.text ?? ''

  return (
    <div className={css.assistantTurnRoot}>
      {/* 1. Collapsible Process Header: e.g. "用时 6分钟 58秒 >" */}
      <div className={css.processHeaderRow}>
        <button
          type="button"
          className={css.processHeaderBtn}
          onClick={() => setUserToggled(!open)}
          aria-expanded={open}
          title={open ? '收起执行过程与思考' : '展开执行过程与思考'}
        >
          <span>{headerLabel}</span>
          <span className={css.chevronIcon}>
            {open ? (
              <IconChevronDownOutlineRegular size={13} />
            ) : (
              <IconChevronRightOutlineRegular size={13} />
            )}
          </span>
        </button>
      </div>

      {/* 2. Expanded Process Container (Thinking + Tools) - Borderless like Figure 2 */}
      {open && (
        <div className={css.processContainer}>
          {hasReasoning && turn.assistant && (
            <div className={css.reasoningText}>
              <SmoothMarkdown
                text={turn.assistant.reasoning}
                labels={labels}
                streaming={turn.assistant.streaming}
                variant="compact"
              />
            </div>
          )}

          {hasTools && (
            <div className={css.toolsList}>
              {turn.tools.map((tool) => (
                <Item
                  key={tool.key}
                  item={tool}
                  turnClosed={turn.closed}
                  slashNames={slashNames}
                  onOpenTask={onOpenTask}
                />
              ))}
            </div>
          )}

          {!hasReasoning && !hasTools && (
            <div style={{ color: 'var(--dsw-alias-label-tertiary, #9ca3af)', fontSize: 12, padding: '4px 0' }}>
              （本轮未产生额外的思考或外部工具调用）
            </div>
          )}
        </div>
      )}

      {/* 3. Conclusion (Final Answer Text) */}
      {conclusionText !== '' && (
        <div className={css.conclusionBody}>
          <AssistantMessage
            text={conclusionText}
            reasoning=""
            streaming={turn.assistant?.streaming}
          />
        </div>
      )}

      {/* 4. In-flight Indicator when concluding text hasn't arrived yet (Figure 2) */}
      {isTurnRunning && conclusionText === '' && (
        <div className={css.inFlightAnalyzing}>
          <TextShimmer active className={css.thinking}>
            正在分析请求
          </TextShimmer>
        </div>
      )}

      {/* 6. Codex-Style Edited Files Summary Card */}
      {changes && <ChangesCard taskId={taskId} summary={changes} running={running} onChangesUpdated={onChangesUpdated} />}
      {trackingError && <p role="status" className={css.turnNote}>部分改动未能记录：{trackingError}</p>}
      {!changes && turn.tools.some(item => item.kind === 'tool' && /^(write_file|replace_range|edit_file)$/.test(item.name) && item.result && !item.result.isError) && <p className={css.turnNote}>这轮没有可用的修改前后快照，无法显示准确统计或撤销。</p>}
      {/* 7. Error or Abort notices if any */}
      {turn.turnEnd && turn.turnEnd.reason.kind !== 'completed' && (
        <TurnEnd reason={turn.turnEnd.reason} durationMs={turn.durationMs} />
      )}
    </div>
  )
}

type ChatNode =
  | { kind: 'user'; item: ChatItem & { kind: 'user' } }
  | { kind: 'assistant-turn'; turn: AssistantTurnData }
  | { kind: 'turn-end'; item: ChatItem & { kind: 'turn-end' } }

/** Transcript column using ui-chat's ChatView frame, column width, and flow gap; the conversation scroll body scrolls it. */
export function ChatView({ events, task, running, onOpenTask, onChangesUpdated }: {
  events: readonly AgentEvent[]
  task?: Task | undefined
  running: boolean
  onOpenTask: (taskId: string) => void
  onChangesUpdated?: (() => void) | undefined
}) {
  const api = useAgentApi()
  const [slashNames, setSlashNames] = useState<readonly string[]>([])
  useEffect(() => {
    api.commands().then(
      (body) => { setSlashNames(body.commands.map((command) => command.name)) },
      () => {},
    )
  }, [api])

  const items = useMemo(() => chatItems(events, task), [events, task])
  const changes = useMemo(() => changeSummaries(events), [events])
  const trackingErrors = useMemo(() => {
    const map = new Map<number, string>()
    for (const event of events) if (event.type === 'workspace/changes_error') map.set(Number(event.data.turn), String(event.data.message))
    return map
  }, [events])
  const durationInfo = useTaskDuration(events, task, running)
  const thinking = running && awaitingModel(events)

  const closedBefore = useMemo(() => {
    let lastEnd = -1
    items.forEach((item, index) => { if (item.kind === 'turn-end') lastEnd = index })
    return lastEnd
  }, [items])

  const nodes = useMemo<ChatNode[]>(() => {
    const list: ChatNode[] = []
    let currentTurn: AssistantTurnData | null = null
    let turnNumber = 1

    const flushTurn = () => {
      if (currentTurn) {
        if (
          currentTurn.assistant ||
          currentTurn.tools.length > 0 ||
          !currentTurn.closed
        ) {
          list.push({ kind: 'assistant-turn', turn: currentTurn })
        } else if (currentTurn.turnEnd) {
          list.push({ kind: 'turn-end', item: currentTurn.turnEnd })
        }
        currentTurn = null
      }
    }

    items.forEach((item, index) => {
      if (item.kind === 'turn') {
        flushTurn()
        turnNumber = item.turn
        return
      }

      if (item.kind === 'user') {
        flushTurn()
        list.push({ kind: 'user', item })
        return
      }

      if (item.kind === 'tool' || item.kind === 'subagent' || item.kind === 'progress' || item.kind === 'observer') {
        if (!currentTurn) {
          currentTurn = {
            key: `turn:${item.key}`,
            turn: turnNumber,
            tools: [],
            closed: index < closedBefore || (!running && item.kind === 'tool'),
          }
        }
        currentTurn.tools.push(item)
        return
      }

      if (item.kind === 'assistant') {
        if (!currentTurn) {
          currentTurn = {
            key: `turn:${item.key}`,
            turn: turnNumber,
            tools: [],
            closed: index < closedBefore,
          }
        }
        currentTurn.assistant = item
        return
      }

      if (item.kind === 'turn-end') {
        if (currentTurn) {
          currentTurn.turnEnd = item
          currentTurn.closed = true
          currentTurn.durationMs = item.durationMs
          flushTurn()
        } else {
          list.push({ kind: 'turn-end', item })
        }
        return
      }
    })

    flushTurn()
    return list
  }, [items, closedBefore, running])

  return (
    <div className={chatCss.frame}>
      <div className={chatCss.root}>
        <div className={chatCss.scroll}>
          <div className={chatCss.column}>
            {items.length === 0 && !thinking && <div className={chatCss.hint}>{t('app.waiting')}</div>}

            {nodes.map((node) => {
              if (node.kind === 'user') {
                return (
                  <div key={node.item.key} className={chatCss.flowItem} data-chat-flow-kind="user">
                    <Item
                      item={node.item}
                      turnClosed={true}
                      slashNames={slashNames}
                      onOpenTask={onOpenTask}
                    />
                  </div>
                )
              }

              if (node.kind === 'assistant-turn') {
                return (
                  <div key={node.turn.key} className={chatCss.flowItem} data-chat-flow-kind="assistant">
                    <AssistantTurnView
                      turn={node.turn}
                      taskId={task?.task_id ?? ''}
                      changes={changes.get(node.turn.turn)}
                      trackingError={trackingErrors.get(node.turn.turn)}
                      onChangesUpdated={onChangesUpdated}
                      running={running}
                      liveDurationMs={durationInfo.currentTurnDurationMs}
                      slashNames={slashNames}
                      onOpenTask={onOpenTask}
                    />
                  </div>
                )
              }

              return (
                <div key={node.item.key} className={chatCss.flowItem} data-chat-flow-kind={node.item.kind}>
                  <Item
                    item={node.item}
                    turnClosed={true}
                    slashNames={slashNames}
                    onOpenTask={onOpenTask}
                  />
                </div>
              )
            })}

            {thinking && nodes.length === 0 && (
              <div className={chatCss.flowItem}>
                <TextShimmer active className={css.thinking}>
                  {durationInfo.currentTurnDurationMs !== undefined
                    ? `${t('message.stepProcess.thinking')} · 已用时 ${formatDuration(durationInfo.currentTurnDurationMs)}`
                    : t('message.stepProcess.thinking')}
                </TextShimmer>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
