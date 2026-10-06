import { useState } from 'react'
import { Modal } from '../dsh/ui-primitives/Modal.tsx'
import { Button } from '../dsh/ui-primitives/Button.tsx'
import { IconFolderOpenRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import heroCss from '../dsh/conversation/HeroShell.module.css'
import { workspaceName } from './Sidebar.tsx'

export function WorkspaceModal({
  open,
  onClose,
  onAddWorkspace,
  existingWorkspaces = [],
}: {
  open: boolean
  onClose: () => void
  onAddWorkspace: (path: string) => void
  existingWorkspaces?: readonly string[]
}) {
  const [pathInput, setPathInput] = useState('')
  const [error, setError] = useState<string | null>(null)

  const handleNativePick = async () => {
    if ('showDirectoryPicker' in window) {
      try {
        const dirHandle = await (window as unknown as { showDirectoryPicker: () => Promise<{ name: string }> }).showDirectoryPicker()
        if (dirHandle && dirHandle.name) {
          onAddWorkspace(dirHandle.name)
          onClose()
          return
        }
      } catch (err: unknown) {
        const errorObj = err as { name?: string; message?: string }
        if (errorObj?.name === 'AbortError') return
        setError(errorObj?.message ?? String(err))
      }
    } else {
      setError('当前浏览器不支持直接唤起原生文件夹选择器，请在下方直接输入或粘贴项目文件夹路径。')
    }
  }

  const handleConfirm = () => {
    const trimmed = pathInput.trim()
    if (!trimmed) {
      setError('请输入文件夹或项目路径')
      return
    }
    setError(null)
    onAddWorkspace(trimmed)
    setPathInput('')
    onClose()
  }

  const SUGGESTED_PROJECTS = [
    'd:\\enterpriseProject\\codex-workspace-mcp',
    'd:\\enterpriseProject\\pptx-editor-engine',
    'd:\\enterpriseProject\\deepseek-harness',
    'd:\\enterpriseProject\\woc-wnas',
    'd:\\enterpriseProject\\ai-ppt-server',
  ]

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="选择项目文件夹"
      closeLabel="关闭"
      description="选择本地目录或输入项目文件夹路径以切换或添加工作区"
      footer={(
        <>
          <Button variant="outline" className={heroCss.modalAction} onClick={onClose}>
            取消
          </Button>
          <Button variant="primary" className={heroCss.modalAction} onClick={handleConfirm}>
            确认选择
          </Button>
        </>
      )}
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
        {/* Native directory picker button */}
        {'showDirectoryPicker' in window && (
          <div>
            <Button
              variant="outline"
              size="md"
              icon={<IconFolderOpenRegular size={16} />}
              style={{ width: '100%', justifyContent: 'center', gap: 8, height: 38 }}
              onClick={handleNativePick}
            >
              打开本地文件管理器选择文件夹...
            </Button>
          </div>
        )}

        <div>
          <label style={{ display: 'block', fontSize: 13, fontWeight: 500, marginBottom: 6, color: 'var(--dsw-alias-label-primary, #18181b)' }}>
            输入或粘贴文件夹路径
          </label>
          <input
            className={heroCss.modalInput}
            placeholder="例如: D:\enterpriseProject\pptx-editor-engine"
            value={pathInput}
            onChange={(e) => {
              setPathInput(e.target.value)
              setError(null)
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter') handleConfirm()
            }}
            autoFocus
          />
          {error && <div className={heroCss.modalError} role="alert">{error}</div>}
        </div>

        {/* Quick project suggestions */}
        <div>
          <div style={{ fontSize: 12, color: 'var(--dsw-alias-label-secondary, #71717a)', marginBottom: 6 }}>
            快速选择现有项目:
          </div>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
            {SUGGESTED_PROJECTS.map((proj) => {
              const name = workspaceName(proj)
              const isCurrent = existingWorkspaces.includes(proj)
              return (
                <button
                  key={proj}
                  type="button"
                  onClick={() => {
                    onAddWorkspace(proj)
                    onClose()
                  }}
                  style={{
                    display: 'inline-flex',
                    alignItems: 'center',
                    gap: 5,
                    padding: '4px 10px',
                    borderRadius: 14,
                    border: '1px solid var(--codex-card-border, #e5e7eb)',
                    background: isCurrent ? 'var(--dsw-alias-interactive-bg-hover, #e4e4e7)' : 'var(--codex-hover-bg, #f4f4f5)',
                    color: 'var(--codex-text-primary, #18181b)',
                    fontSize: 12,
                    cursor: 'pointer',
                  }}
                >
                  <IconFolderOpenRegular size={13} />
                  <span>{name}</span>
                </button>
              )
            })}
          </div>
        </div>
      </div>
    </Modal>
  )
}
