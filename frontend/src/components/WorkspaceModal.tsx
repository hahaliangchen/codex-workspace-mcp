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
  onAddWorkspace: (path: string) => Promise<void>
  existingWorkspaces?: readonly string[]
}) {
  const [pathInput, setPathInput] = useState('')
  const [error, setError] = useState<string | null>(null)

  const [busy, setBusy] = useState(false)
  const handleConfirm = async (path = pathInput) => {
    if (!path.trim()) { setError('请输入项目目录的完整路径'); return }
    setBusy(true)
    setError(null)
    try { await onAddWorkspace(path.trim()); setPathInput(''); onClose() }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)) }
    finally { setBusy(false) }
  }

  return (
    <Modal
      open={open}
      onClose={onClose}
      title="选择项目文件夹"
      closeLabel="关闭"
      description="选择本地目录或输入项目文件夹路径以切换或添加工作区"
      footer={(
        <>
          <Button variant="outline" className={heroCss.modalAction} disabled={busy} onClick={onClose}>
            取消
          </Button>
          <Button variant="primary" className={heroCss.modalAction} disabled={busy} onClick={() => void handleConfirm()}>
            确认选择
          </Button>
        </>
      )}
    >
      <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>

        <div>
          <label style={{ display: 'block', fontSize: 13, fontWeight: 500, marginBottom: 6, color: 'var(--dsw-alias-label-primary, #18181b)' }}>
            输入或粘贴文件夹路径
          </label>
          <input
            className={heroCss.modalInput}
            placeholder="输入服务端的完整目录路径"
            value={pathInput}
            onChange={(e) => {
              setPathInput(e.target.value)
              setError(null)
            }}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && !busy) void handleConfirm()
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
            {existingWorkspaces.map((proj) => {
              const name = workspaceName(proj)
              const isCurrent = existingWorkspaces.includes(proj)
              return (
                <button
                  key={proj}
                  type="button"
                  onClick={() => {
                    if (!busy) void handleConfirm(proj)
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
