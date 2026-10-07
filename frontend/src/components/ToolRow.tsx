import { memo, useMemo, type ReactNode } from 'react'
import clsx from 'clsx'
import {
  DiffBlock, DisclosureRow, IconCordisPluginOutlineRegular,
  IconDatabaseOutlineRegular,
  IconFlatListOutlineRegular, IconUsersOutlineRegular,
  IconWorkspaceTreeOutlineRegular, ReadBlock, SearchBlock, TerminalBlock,
  TextShimmer, diffTotals, languageForPath,
  type DiffBlockLabels, type DiffHunk, type ReadBlockLabels, type ReadBlockLine,
  type SearchBlockLabels, type TerminalBlockLabels,
  type SearchFileGroup, type SearchBlockLineMatch,
} from '@deepseek-ai/dsh-client-ui-primitives'
import {
  FolderExplorerBlock,
  GenericIoCard,
  ParamBadgesBar,
  extractFolderEntries,
  extractParamBadges,
  extractSearchMatches,
} from './CodexToolCard.tsx'
import { useDisclosure } from '../disclosure.ts'
import css from '../dsh/tool/ToolRow.module.css'
import { t } from '../i18n.ts'
import type { ToolResult } from '../model.ts'
import { VisualArtifactPreview } from './VisualMaterials.tsx'
import type { VisualRecord } from '../visual.ts'

const SUMMARY_FIELDS = ['path', 'file_path', 'TargetFile', 'AbsolutePath', 'query', 'pattern', 'command', 'CommandLine', 'program', 'symbol', 'symbol_id', 'name', 'prompt', 'area']

function iconFor(name: string): ReactNode {
  if (name === 'run_program' || name === 'run_command' || /bash|shell|exec|powershell|cmd/.test(name)) {
    return (
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
        <rect x="1.5" y="1.5" width="13" height="13" rx="3" stroke="currentColor" />
        <path d="M4.5 5.5L7 8L4.5 10.5" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" />
        <line x1="8.5" y1="10.5" x2="11.5" y2="10.5" stroke="currentColor" strokeLinecap="round" />
      </svg>
    )
  }
  if (/write|edit|replace|apply|patch|create|delete/.test(name)) {
    return (
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
        <rect x="1.5" y="1.5" width="13" height="13" rx="3" stroke="currentColor" />
        <path d="M5.5 10.5L10 6L9 5L4.5 9.5V11.5H6.5L5.5 10.5Z" stroke="currentColor" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    )
  }
  if (/read|open|view/.test(name)) {
    return (
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
        <rect x="2" y="1.5" width="12" height="13" rx="2.5" stroke="currentColor" />
        <path d="M5 5H11M5 8H11M5 11H8.5" stroke="currentColor" strokeLinecap="round" />
      </svg>
    )
  }
  if (/search|find|grep/.test(name)) {
    return (
      <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3">
        <rect x="1.5" y="1.5" width="13" height="13" rx="3" stroke="currentColor" />
        <circle cx="7.5" cy="7.5" r="2.8" stroke="currentColor" />
        <line x1="9.5" y1="9.5" x2="12" y2="12" stroke="currentColor" strokeLinecap="round" />
      </svg>
    )
  }
  if (/symbol|index/.test(name)) return <IconWorkspaceTreeOutlineRegular size={14} />
  if (name === 'spawn_subagent') return <IconUsersOutlineRegular size={14} />
  if (/memory|architecture|business/.test(name)) return <IconDatabaseOutlineRegular size={14} />
  if (/list/.test(name)) return <IconFlatListOutlineRegular size={14} />
  return <IconCordisPluginOutlineRegular size={14} />
}

function titleFor(name: string): string {
  if (name === 'run_program') return '运行了程序'
  if (name === 'run_command' || /bash|shell|exec|powershell|cmd/.test(name)) return '运行了命令'
  if (/write|edit|replace|apply|patch/.test(name)) return '编辑了文件'
  if (/read|view_file/.test(name)) return '读取了文件'
  if (/search|grep/.test(name)) return '搜索了代码'
  if (/symbol|index/.test(name)) return '检索了符号'
  if (name === 'spawn_subagent') return '启动了子任务'
  if (/list|find/.test(name)) return '浏览了目录'
  if (/memory|architecture|business/.test(name)) return '读取了工作区记忆'
  return name
}

function parse(raw: string): unknown {
  try { return JSON.parse(raw) } catch { return raw }
}

function summaryOf(args: unknown): string {
  if (typeof args !== 'object' || args === null) return typeof args === 'string' ? args : ''
  const record = args as Record<string, unknown>
  for (const field of SUMMARY_FIELDS) {
    const value = record[field]
    if (typeof value === 'string' && value !== '') return value.replace(/\s+/g, ' ')
  }
  return ''
}

function extractDiffHunks(name: string, args: unknown, result: ToolResult | undefined): DiffHunk[] | undefined {
  if (!result || result.isError) return undefined
  try { if ((JSON.parse(result.text) as { changed?: boolean }).changed === false) return undefined } catch { /* non-JSON tool output */ }
  if (!/write|edit|replace|apply|patch/.test(name)) return undefined
  if (typeof args !== 'object' || args === null) return undefined
  const rec = args as Record<string, unknown>
  const path = String(rec.path ?? rec.file_path ?? rec.TargetFile ?? '')
  if (!path) return undefined
  if (name === 'replace_range' && typeof rec.expected_old_text === 'string' && typeof rec.replacement === 'string') {
    return [{ path, oldText: rec.expected_old_text, newText: rec.replacement }]
  }
  if (typeof rec.old_str === 'string' || typeof rec.new_str === 'string') {
    return [{
      path,
      oldText: typeof rec.old_str === 'string' ? rec.old_str : null,
      newText: typeof rec.new_str === 'string' ? rec.new_str : '',
    }]
  }
  if (typeof rec.TargetContent === 'string' || typeof rec.ReplacementContent === 'string') {
    return [{
      path,
      oldText: typeof rec.TargetContent === 'string' ? rec.TargetContent : null,
      newText: typeof rec.ReplacementContent === 'string' ? rec.ReplacementContent : '',
    }]
  }
  // A whole-file write argument is not proof that the file was newly created.
  // Full before/after comparisons live in the server-recorded turn review.
  return undefined
}

function extractReadLines(name: string, args: unknown, result: ToolResult | undefined): {
  filePath: string
  lines: ReadBlockLine[]
  lang?: string | undefined
} | undefined {
  if (result === undefined || result.isError) return undefined
  if (!/read|view|cat|symbol|get_code|inspect/.test(name)) return undefined
  if (typeof args !== 'object' || args === null) return undefined
  const rec = args as Record<string, unknown>

  let filePath = String(
    rec.path ??
    rec.file_path ??
    rec.filePath ??
    rec.TargetFile ??
    rec.AbsolutePath ??
    rec.file ??
    rec.uri ??
    ''
  )
  let startLine = typeof rec.start_line === 'number'
    ? rec.start_line
    : typeof rec.startLine === 'number'
      ? rec.startLine
      : typeof rec.StartLine === 'number'
        ? rec.StartLine
        : 1

  let langHint: string | undefined

  if (!filePath) {
    const symbolId = String(rec.symbol_id ?? rec.symbolId ?? rec.symbol ?? '')
    if (symbolId) {
      const parts = symbolId.split(':')
      if (parts.length >= 2) {
        if (/^(rust|go|ts|js|python|py|tsx|jsx|rs|cpp|c|java|kotlin|swift)$/i.test(parts[0]!)) {
          const prefix = parts[0]!.toLowerCase()
          langHint = prefix === 'rs' ? 'rust' : prefix === 'py' ? 'python' : prefix
          filePath = parts[1] ?? ''
          if (parts[3] && /^\d+$/.test(parts[3])) {
            startLine = parseInt(parts[3], 10)
          } else if (parts[2] && /^\d+$/.test(parts[2])) {
            startLine = parseInt(parts[2], 10)
          }
        } else {
          filePath = parts[0] ?? ''
          if (parts[2] && /^\d+$/.test(parts[2])) {
            startLine = parseInt(parts[2], 10)
          } else if (parts[1] && /^\d+$/.test(parts[1])) {
            startLine = parseInt(parts[1], 10)
          }
        }
      } else {
        filePath = symbolId
      }
    }
  }

  if (!filePath) return undefined

  const parsedOut = parse(result.text)
  let contentText = ''
  if (typeof parsedOut === 'object' && parsedOut !== null) {
    const recOut = parsedOut as Record<string, unknown>
    if (typeof recOut.content === 'string') {
      contentText = recOut.content
    } else if (typeof recOut.code === 'string') {
      contentText = recOut.code
    } else if (typeof recOut.text === 'string') {
      contentText = recOut.text
    } else if (Array.isArray(recOut.lines)) {
      contentText = recOut.lines
        .map(l => (typeof l === 'object' && l !== null && 'text' in l ? String((l as { text: unknown }).text) : String(l)))
        .join('\n')
    }
    if (typeof recOut.start_line === 'number') {
      startLine = recOut.start_line
    } else if (typeof recOut.line === 'number') {
      startLine = recOut.line
    }
  } else if (typeof result.text === 'string') {
    const trimmed = result.text.trim()
    if (!trimmed.startsWith('{') || !trimmed.endsWith('}')) {
      contentText = result.text
    }
  }

  if (!contentText) return undefined

  const rawLines = contentText.replace(/\r\n/g, '\n').split('\n')
  if (rawLines.length > 1 && rawLines[rawLines.length - 1] === '') rawLines.pop()
  const lines: ReadBlockLine[] = rawLines.map((lineText, idx) => {
    const numbered = /^(\d+)[:|]\s?(.*)$/.exec(lineText)
    if (numbered !== null) {
      return { number: Number(numbered[1]), text: numbered[2] ?? '' }
    }
    return { number: startLine + idx, text: lineText }
  })

  const lang = languageForPath(filePath) ?? langHint ?? (
    name.includes('rust') ? 'rust' :
    name.includes('go') ? 'go' :
    name.includes('python') ? 'python' :
    name.includes('ts') ? 'typescript' :
    undefined
  )

  return { filePath, lines, lang }
}

/** ui-tool ToolRow with authentic DSH TerminalBlock, ReadBlock, DiffBlock, and SearchBlock surfaces. */
export const ToolRow = memo(function ToolRow({ name, rawArguments, result, stopped, onOpenChild }: {
  name: string
  rawArguments: string
  result: ToolResult | undefined
  /** The turn ended without this call producing a result. */
  stopped: boolean
  onOpenChild: (taskId: string) => void
}) {
  const { expanded, toggle } = useDisclosure()
  const args = useMemo(() => parse(rawArguments), [rawArguments])
  const state = result === undefined ? (stopped ? 'stopped' : 'running') : result.isError ? 'error' : 'ok'
  const running = state === 'running'
  const nativeParsed = name === 'run_program' && result ? parse(result.text) : undefined
  const nativeResult = typeof nativeParsed === 'object' && nativeParsed !== null ? nativeParsed as Record<string, unknown> : undefined
  const nativeDiagnostics = nativeResult
    ? [
        ...(['stdout', 'stderr'] as const).flatMap((stream) => {
          const decodeError = nativeResult[`${stream}_decode_error`]
          const readError = nativeResult[`${stream}_read_error`]
          return [
            typeof decodeError === 'string' && decodeError.length > 0
              ? `${stream} UTF-8 解码失败：${decodeError}（原始字节保留在 ${stream}_raw_base64）`
              : undefined,
            nativeResult[`${stream}_truncated`] === true ? `${stream} 输出已截断` : undefined,
            typeof readError === 'string' && readError.length > 0 ? `${stream} 管道读取失败：${readError}` : undefined,
          ]
        }),
      ].filter((message): message is string => typeof message === 'string')
    : []
  const visualArtifact = useMemo(() => {
    const value = result ? parse(result.text) as VisualRecord : null
    return value && typeof value === 'object' && value.visual_artifact ? value.visual_artifact as VisualRecord : undefined
  }, [result])
  const errorLine = result?.isError === true ? result.text.split('\n')[0] ?? '' : ''
  const nativeSummary = nativeResult
    ? nativeResult.outcome === 'exited'
      ? `退出码 ${String(nativeResult.process_exit_code ?? '未知')}${nativeDiagnostics.length > 0 ? ` · ${nativeDiagnostics.length} 项输出诊断` : ''}`
      : `${String(nativeResult.outcome ?? '未启动')}${nativeResult.error_message ? ` · ${String(nativeResult.error_message)}` : ''}${nativeDiagnostics.length > 0 ? ` · ${nativeDiagnostics.length} 项输出诊断` : ''}`
    : ''
  const summaryText = name === 'run_program' ? nativeSummary : errorLine !== '' ? errorLine : summaryOf(args)
  const duration = result?.durationMs === undefined ? null : `${result.durationMs} ms`

  const diffs = useMemo(() => extractDiffHunks(name, args, result), [name, args, result])
  const diffStats = useMemo(() => (diffs ? diffTotals(diffs) : undefined), [diffs])
  const readData = useMemo(() => extractReadLines(name, args, result), [name, args, result])
  const paramBadges = useMemo(() => extractParamBadges(args), [args])
  const folderEntries = useMemo(() => (result && !result.isError ? extractFolderEntries(result.text) : null), [result])
  const searchMatchesData = useMemo(() => (result && !result.isError ? extractSearchMatches(result.text) : null), [result])

  const searchGroups = useMemo<SearchFileGroup[] | undefined>(() => {
    if (!searchMatchesData || searchMatchesData.matches.length === 0) return undefined
    const map = new Map<string, SearchBlockLineMatch[]>()
    for (const m of searchMatchesData.matches) {
      const list = map.get(m.path) ?? []
      list.push({ lineNumber: m.line ?? 1, line: m.preview ?? '' })
      map.set(m.path, list)
    }
    return Array.from(map.entries()).map(([path, matches]) => ({ path, matches }))
  }, [searchMatchesData])

  const terminalLabels = useMemo<TerminalBlockLabels>(() => ({
    signal: sig => `信号 ${sig}`,
    exitCode: code => `退出码 ${code}`,
    noExitCode: '已终止',
    running: '运行中',
    failed: '失败',
    done: '完成',
    copy: '复制',
    copied: '已复制',
    noOutput: '（无输出）',
    collapseAria: '收起输出',
    collapse: '收起',
    expandAria: hidden => `展开其余 ${hidden} 行`,
    expand: hidden => `⋯ 展开其余 ${hidden} 行`,
  }), [])

  const readLabels = useMemo<ReadBlockLabels>(() => ({
    window: (shown, total) => `显示 ${shown} / 共 ${total} 行`,
    copy: '复制',
    copied: '已复制',
    codeLabel: '文件',
    wrapLabel: '自动换行',
    unwrapLabel: '取消换行',
    collapseAria: '收起代码',
    expandAria: hidden => `展开其余 ${hidden} 行`,
    collapse: '收起',
    expand: hidden => `⋯ 展开其余 ${hidden} 行`,
  }), [])

  const diffLabels = useMemo<DiffBlockLabels>(() => ({
    copy: '复制补丁',
    copied: '已复制',
    codeLabel: '变更',
    wrapLabel: '自动换行',
    unwrapLabel: '取消换行',
    collapseAria: '收起变更',
    expandAria: hidden => `展开其余 ${hidden} 行`,
    collapse: '收起',
    expand: hidden => `⋯ 展开其余 ${hidden} 行`,
  }), [])

  const searchLabels = useMemo<SearchBlockLabels>(() => ({
    pathsSummary: (shown, total, trunc) => trunc ? `显示 ${shown} / 共 ${total} 个路径` : `${shown} 个路径`,
    matchesSummary: (shown, total, files, trunc) => trunc ? `显示 ${shown} / 共 ${total} 处匹配 · ${files} 个文件` : `${shown} 处匹配 · ${files} 个文件`,
    copy: '复制',
    copied: '已复制',
    noResults: '未找到匹配项',
    collapseAria: '收起结果',
    expandAria: hidden => `展开其余 ${hidden} 项`,
    collapse: '收起',
    expand: hidden => `⋯ 展开其余 ${hidden} 项`,
  }), [])

  const collapsedContent = (summaryText !== '' || duration !== null || diffStats !== undefined) && (
    <>
      <span className={css.sep} aria-hidden>·</span>
      <span className={clsx(css.summary, state === 'error' && css.errorSummary, state === 'stopped' && css.stoppedSummary)}>
        <TextShimmer active={running}>{summaryText}</TextShimmer>
      </span>
      {diffStats !== undefined && (diffStats.added > 0 || diffStats.removed > 0) && (
        <span className={css.diffStat}>
          {diffStats.added > 0 && <span className={css.diffAdd}>+{diffStats.added}</span>}
          {diffStats.removed > 0 && <span className={css.diffDel}>-{diffStats.removed}</span>}
        </span>
      )}
      {duration !== null && state !== 'error' && <span className={css.summarySuffix}>{duration}</span>}
    </>
  )

  const childTaskId = result?.childTaskId
  const isCommand = name === 'run_program' || name === 'run_command' || /bash|shell|exec/.test(name)
  const cmdRecord = typeof args === 'object' && args !== null ? (args as Record<string, unknown>) : {}
  const nativeArgs = Array.isArray(cmdRecord.args) ? cmdRecord.args.map(value => JSON.stringify(String(value))).join(' ') : ''
  const commandStr = name === 'run_program'
    ? `${String(cmdRecord.program ?? '')}${nativeArgs ? ` ${nativeArgs}` : ''}`
    : String(cmdRecord.command ?? cmdRecord.CommandLine ?? summaryText)
  const cwdStr = typeof cmdRecord.project_path === 'string'
    ? cmdRecord.project_path
    : typeof cmdRecord.cwd === 'string'
    ? cmdRecord.cwd
    : typeof cmdRecord.Cwd === 'string'
      ? cmdRecord.Cwd
      : undefined
  const nativeOutput = nativeResult
    ? [nativeResult.stdout, nativeResult.stderr, nativeResult.error_message ? `Error: ${String(nativeResult.error_message)}` : undefined,
        ...nativeDiagnostics.map((message) => `Output diagnostic: ${message}`)]
      .filter((part): part is string => typeof part === 'string' && part.length > 0).join('\n')
    : undefined
  const nativeExitCode = typeof nativeResult?.process_exit_code === 'number' ? nativeResult.process_exit_code : null

  return (
    <div className={css.root} data-variant="generic" data-tool={name} data-state={state}>
      {running && <span className={css.visuallyHidden}>{t('row.running')}</span>}
      <DisclosureRow
        rowClassName={css.row}
        leadingClassName={css.leading}
        titleClassName={css.title}
        chevronClassName={css.chevron}
        icon={iconFor(name)}
        title={titleFor(name)}
        running={running}
        open={expanded}
        expandable
        expandOnRowClick
        keepContentWhenOpen
        onToggle={toggle}
        collapsedContent={collapsedContent}
      >
        {expanded && (
          <div className={css.bodyWrap}>
            {visualArtifact && <VisualArtifactPreview artifact={visualArtifact} />}
            {isCommand
              ? (
                  <TerminalBlock
                    command={commandStr}
                    cwd={cwdStr}
                    output={name === 'run_program' ? nativeOutput : result?.text}
                    exitCode={name === 'run_program' ? nativeExitCode : result === undefined ? undefined : result.isError ? 1 : 0}
                    running={running}
                    labels={terminalLabels}
                  />
                )
              : diffs !== undefined && diffs.length > 0
                ? (
                    <DiffBlock
                      diffs={diffs}
                      labels={diffLabels}
                    />
                  )
                : readData !== undefined
                  ? (
                      <ReadBlock
                        label={readData.filePath}
                        lines={readData.lines}
                        totalLines={readData.lines.length}
                        lang={readData.lang ?? languageForPath(readData.filePath)}
                        labels={readLabels}
                      />
                    )
                  : searchMatchesData !== null
                    ? (
                        <div style={{ display: 'flex', flexDirection: 'column', gap: 6, margin: '4px 0' }}>
                          {paramBadges.length > 0 && <ParamBadgesBar badges={paramBadges} />}
                          <SearchBlock
                            kind="matches"
                            files={searchGroups ?? []}
                            total={searchMatchesData.total}
                            truncated={false}
                            labels={searchLabels}
                          />
                        </div>
                      )
                    : folderEntries !== null
                      ? (
                          <div style={{ display: 'flex', flexDirection: 'column', gap: 6, margin: '4px 0' }}>
                            {paramBadges.length > 0 && <ParamBadgesBar badges={paramBadges} />}
                            <FolderExplorerBlock
                              entries={folderEntries}
                              query={typeof args === 'object' && args !== null ? ((args as Record<string, unknown>).query as string) : undefined}
                            />
                          </div>
                        )
                      : (
                          <GenericIoCard
                            args={args}
                            resultText={result?.text}
                            isError={result?.isError}
                            badges={paramBadges}
                          />
                        )}
            {childTaskId !== undefined && (
              <button type="button" className={css.inspectButton} onClick={() => { onOpenChild(childTaskId) }}>
                {t('app.viewSubagent')}
              </button>
            )}
          </div>
        )}
      </DisclosureRow>
    </div>
  )
})
