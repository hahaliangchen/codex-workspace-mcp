import { memo, useMemo, useState, type ReactNode } from 'react'
import clsx from 'clsx'
import css from './CodexToolCard.module.css'

export interface FolderEntry {
  path: string
  kind?: 'dir' | 'file' | 'symlink' | undefined
  size_bytes?: number | undefined
}

export function formatBytes(bytes?: number): string | undefined {
  if (bytes === undefined || bytes === null || isNaN(bytes)) return undefined
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`
}

export function splitPath(fullPath: string): { prefix: string; name: string } {
  const norm = fullPath.replace(/\\/g, '/')
  const lastSlash = norm.lastIndexOf('/')
  if (lastSlash === -1) {
    return { prefix: '', name: norm }
  }
  return {
    prefix: norm.slice(0, lastSlash + 1),
    name: norm.slice(lastSlash + 1),
  }
}

export function extractFolderEntries(rawOutput: unknown): FolderEntry[] | null {
  if (!rawOutput) return null
  let data = rawOutput
  if (typeof data === 'string') {
    const trimmed = data.trim()
    if ((trimmed.startsWith('{') && trimmed.endsWith('}')) || (trimmed.startsWith('[') && trimmed.endsWith(']'))) {
      try {
        data = JSON.parse(trimmed)
      } catch {
        const lines = trimmed.split('\n').map(s => s.trim()).filter(Boolean)
        if (lines.length > 0 && lines.every(l => !l.includes('{') && !l.includes('}'))) {
          return lines.map(p => ({
            path: p,
            kind: p.endsWith('/') ? 'dir' : 'file',
          }))
        }
        return null
      }
    } else {
      const lines = trimmed.split('\n').map(s => s.trim()).filter(Boolean)
      if (lines.length > 0 && lines.every(l => !l.startsWith('{') && !l.endsWith('}'))) {
        return lines.map(p => ({
          path: p,
          kind: p.endsWith('/') ? 'dir' : 'file',
        }))
      }
      return null
    }
  }

  if (Array.isArray(data)) {
    if (data.length === 0) return []
    const entries: FolderEntry[] = []
    for (const item of data) {
      if (typeof item === 'string') {
        entries.push({ path: item, kind: item.endsWith('/') ? 'dir' : 'file' })
      } else if (typeof item === 'object' && item !== null && 'path' in item) {
        entries.push({
          path: String((item as Record<string, unknown>).path),
          kind: (item as Record<string, unknown>).kind as 'dir' | 'file' | undefined,
          size_bytes: typeof (item as Record<string, unknown>).size_bytes === 'number'
            ? (item as Record<string, unknown>).size_bytes as number
            : undefined,
        })
      } else {
        return null
      }
    }
    return entries
  }

  if (typeof data === 'object' && data !== null) {
    const obj = data as Record<string, unknown>
    if (Array.isArray(obj.entries)) {
      return extractFolderEntries(obj.entries)
    }
    if (Array.isArray(obj.files)) {
      return extractFolderEntries(obj.files)
    }
    if (Array.isArray(obj.paths)) {
      return extractFolderEntries(obj.paths)
    }
  }

  return null
}

export interface SearchMatchItem {
  path: string
  line?: number | undefined
  preview?: string | undefined
}

export function extractSearchMatches(rawOutput: unknown): { query?: string | undefined; matches: SearchMatchItem[]; total: number } | null {
  if (!rawOutput) return null
  let data = rawOutput
  if (typeof data === 'string') {
    try {
      data = JSON.parse(data)
    } catch {
      return null
    }
  }
  if (typeof data === 'object' && data !== null) {
    const obj = data as Record<string, unknown>
    if ('matches' in obj && Array.isArray(obj.matches)) {
      const matches: SearchMatchItem[] = []
      for (const item of obj.matches) {
        if (typeof item === 'object' && item !== null && 'path' in item) {
          const rec = item as Record<string, unknown>
          matches.push({
            path: String(rec.path),
            line: typeof rec.line === 'number' ? rec.line : undefined,
            preview: typeof rec.preview === 'string' ? rec.preview : typeof rec.text === 'string' ? rec.text : undefined,
          })
        }
      }
      return {
        query: typeof obj.query === 'string' ? obj.query : undefined,
        matches,
        total: matches.length,
      }
    }
  }
  return null
}

/** Parameter key-value extraction for badges */
export interface ParamBadgeItem {
  icon: string
  label: string
  value: string
}

export function extractParamBadges(args: unknown): ParamBadgeItem[] {
  if (typeof args !== 'object' || args === null) return []
  const rec = args as Record<string, unknown>
  const badges: ParamBadgeItem[] = []

  const path = rec.path ?? rec.file_path ?? rec.TargetFile ?? rec.AbsolutePath ?? rec.cwd ?? rec.Cwd
  if (typeof path === 'string' && path) {
    badges.push({ icon: '📁', label: '路径', value: path })
  }

  const query = rec.query ?? rec.pattern ?? rec.prompt
  if (typeof query === 'string' && query) {
    badges.push({ icon: '🔍', label: '关键词', value: query })
  }

  const cmd = rec.command ?? rec.CommandLine
  if (typeof cmd === 'string' && cmd) {
    badges.push({ icon: '⚡', label: '命令', value: cmd })
  }

  const symbol = rec.symbol ?? rec.symbol_id ?? rec.name
  if (typeof symbol === 'string' && symbol && !cmd && !path) {
    badges.push({ icon: '🏷️', label: '目标', value: symbol })
  }

  if (rec.recursive !== undefined) {
    badges.push({ icon: '🔄', label: '递归', value: rec.recursive ? '开启' : '关闭' })
  }

  if (typeof rec.max_matches === 'number') {
    badges.push({ icon: '🔢', label: '上限', value: String(rec.max_matches) })
  }

  return badges
}

/**
 * Lightweight JSON Syntax Highlighter
 * Colorizes keys, strings, numbers, booleans, and null values with zero dependencies.
 */
export const JsonHighlighter = memo(function JsonHighlighter({ value }: { value: unknown }) {
  const tokens = useMemo(() => {
    let str: string
    if (typeof value === 'string') {
      try {
        str = JSON.stringify(JSON.parse(value), null, 2)
      } catch {
        str = value
      }
    } else {
      str = JSON.stringify(value, null, 2)
    }

    const regex = /("(\\u[a-zA-Z0-9]{4}|\\[^u]|[^\\"])*"(\s*:)?|\b(true|false|null)\b|-?\d+(?:\.\d*)?(?:[eE][+\-]?\d+)?|[{}[\],]|\n|\s+)/g
    const result: ReactNode[] = []
    let match: RegExpExecArray | null
    let keyIdx = 0

    while ((match = regex.exec(str)) !== null) {
      const part = match[0]
      if (part.endsWith(':')) {
        result.push(
          <span key={keyIdx++} className={css.jsonKey}>
            {part.slice(0, -1)}
            <span className={css.jsonPunct}>:</span>
          </span>
        )
      } else if (part.startsWith('"')) {
        result.push(<span key={keyIdx++} className={css.jsonString}>{part}</span>)
      } else if (part === 'true' || part === 'false') {
        result.push(<span key={keyIdx++} className={css.jsonBool}>{part}</span>)
      } else if (part === 'null') {
        result.push(<span key={keyIdx++} className={css.jsonNull}>{part}</span>)
      } else if (/^-?\d/.test(part)) {
        result.push(<span key={keyIdx++} className={css.jsonNumber}>{part}</span>)
      } else if (/[{}[\],]/.test(part)) {
        result.push(<span key={keyIdx++} className={css.jsonPunct}>{part}</span>)
      } else {
        result.push(part)
      }
    }
    return result
  }, [value])

  return <>{tokens}</>
})

/**
 * Folder & File Explorer Block
 * Displays arrays of files/folders in an IDE directory tree style.
 */
export const FolderExplorerBlock = memo(function FolderExplorerBlock({
  entries,
  query,
}: {
  entries: FolderEntry[]
  query?: string | undefined
}) {
  const [copied, setCopied] = useState(false)

  const { dirCount, fileCount } = useMemo(() => {
    let dirs = 0
    let files = 0
    for (const e of entries) {
      if (e.kind === 'dir' || e.path.endsWith('/')) {
        dirs++
      } else {
        files++
      }
    }
    return { dirCount: dirs, fileCount: files }
  }, [entries])

  const copyAll = () => {
    const text = entries.map(e => e.path).join('\n')
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    })
  }

  return (
    <div className={css.folderBlock}>
      <div className={css.folderSummaryBar}>
        <div className={css.folderStatText}>
          <span>共 {entries.length} 个项目</span>
          {dirCount > 0 && <span>· 📁 {dirCount} 个目录</span>}
          {fileCount > 0 && <span>· 📄 {fileCount} 个文件</span>}
          {query && <span>· 匹配: &quot;{query}&quot;</span>}
        </div>
        {entries.length > 0 && (
          <button type="button" className={css.copyActionBtn} onClick={copyAll}>
            {copied ? '已复制' : '复制全部路径'}
          </button>
        )}
      </div>

      {entries.length === 0 ? (
        <div className={css.emptyFolder}>
          <span>未检索到匹配的目录或文件</span>
        </div>
      ) : (
        <div className={css.folderList}>
          {entries.map((entry, index) => {
            const isDir = entry.kind === 'dir' || entry.path.endsWith('/')
            const { prefix, name } = splitPath(entry.path)
            const sizeFormatted = formatBytes(entry.size_bytes)

            return (
              <div key={`${entry.path}-${index}`} className={css.folderItem} title={entry.path}>
                <div className={css.folderItemMain}>
                  <span className={css.itemIcon}>
                    {isDir ? (
                      <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" className={css.iconFolder}>
                        <path d="M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.93a2 2 0 0 1-1.66-.9l-.82-1.2A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13c0 1.1.9 2 2 2Z" />
                      </svg>
                    ) : (
                      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" className={css.iconFile}>
                        <path d="M14.5 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7.5L14.5 2z" />
                        <polyline points="14 2 14 8 20 8" />
                      </svg>
                    )}
                  </span>
                  <div className={css.pathWrapper}>
                    {prefix && <span className={css.pathPrefix}>{prefix}</span>}
                    <span className={css.pathName}>{name}</span>
                  </div>
                </div>

                {sizeFormatted && (
                  <div className={css.itemMeta}>
                    <span className={css.fileSize}>{sizeFormatted}</span>
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
    </div>
  )
})

export function CopyButton({ text, label = '复制' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)
  const onCopy = (e: React.MouseEvent) => {
    e.stopPropagation()
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    })
  }
  return (
    <button type="button" className={css.copyActionBtn} onClick={onCopy}>
      {copied ? '已复制' : label}
    </button>
  )
}

export function ParamBadgesBar({
  badges,
  copyText,
}: {
  badges: ParamBadgeItem[]
  copyText?: string | undefined
}) {
  if (badges.length === 0 && !copyText) return null
  return (
    <div className={css.paramsHeader}>
      <div className={css.badgesGroup}>
        {badges.map((b, i) => (
          <span key={i} className={css.badge} title={`${b.label}: ${b.value}`}>
            <span aria-hidden>{b.icon}</span>
            <span className={css.badgeKey}>{b.label}:</span>
            <span className={css.badgeVal}>{b.value}</span>
          </span>
        ))}
      </div>
      {copyText && <CopyButton text={copyText} />}
    </div>
  )
}

export function GenericIoCard({
  args,
  resultText,
  isError,
  badges,
}: {
  args: unknown
  resultText?: string | undefined
  isError?: boolean | undefined
  badges?: ParamBadgeItem[] | undefined
}) {
  const inputStr = typeof args === 'string' ? args : JSON.stringify(args, null, 2)
  const outputStr = resultText ? (typeof resultText === 'string' ? resultText : JSON.stringify(resultText, null, 2)) : ''

  return (
    <div className={css.card}>
      {badges && badges.length > 0 && (
        <ParamBadgesBar badges={badges} />
      )}

      {/* Input Section */}
      <div className={css.ioSection}>
        <div className={css.ioSectionBar}>
          <span className={css.ioSectionTag}>输入参数 (Input)</span>
          <CopyButton text={inputStr} />
        </div>
        <div className={css.ioSectionContent}>
          <JsonHighlighter value={args} />
        </div>
      </div>

      {resultText !== undefined && (
        <>
          <div className={css.ioDivider} />
          {/* Output Section */}
          <div className={css.ioSection}>
            <div className={css.ioSectionBar}>
              <span className={clsx(css.ioSectionTag, isError && css.ioSectionTagError)}>
                {isError ? '执行失败 (Error)' : '返回结果 (Output)'}
              </span>
              <CopyButton text={outputStr} />
            </div>
            <div className={css.ioSectionContent} style={isError ? { color: '#dc2626' } : undefined}>
              <JsonHighlighter value={resultText} />
            </div>
          </div>
        </>
      )}
    </div>
  )
}
