import { useState } from 'react'
import clsx from 'clsx'
import css from './CodexDock.module.css'

export type DockTab = 'home' | 'history' | 'library' | 'mention'

interface CodexDockProps {
  activeTab?: DockTab
  onSelectTab?: (tab: DockTab) => void
  onNewChat?: () => void
  onOpenSettings?: () => void
  onToggleTheme?: () => void
  dark?: boolean
  userName?: string
}

export function CodexDock({
  activeTab = 'home',
  onSelectTab,
  onNewChat,
  onOpenSettings,
  onToggleTheme,
  dark,
  userName = 'RE',
}: CodexDockProps) {
  const [profileOpen, setProfileOpen] = useState(false)

  return (
    <aside className={css.dock} aria-label="应用主导航">
      <div className={css.topGroup}>
        {/* 1. App Logo / Home */}
        <button
          type="button"
          className={clsx(css.dockBtn, css.homeBtn, activeTab === 'home' && css.active)}
          title="首页 / 新会话"
          onClick={() => {
            onSelectTab?.('home')
            if (activeTab === 'home') onNewChat?.()
          }}
        >
          <svg
            width="18"
            height="18"
            viewBox="0 0 24 24"
            fill="currentColor"
            style={{ display: 'block' }}
          >
            <path d="M12 3L3.5 10.5V20a1 1 0 0 0 1 1h5a1 1 0 0 0 1-1v-5a1 1 0 0 1 1-1h1a1 1 0 0 1 1 1v5a1 1 0 0 0 1 1h5a1 1 0 0 0 1-1V10.5L12 3z" />
          </svg>
        </button>

        {/* 2. History */}
        <button
          type="button"
          className={clsx(css.dockBtn, activeTab === 'history' && css.active)}
          title="历史记录"
          onClick={() => onSelectTab?.('history')}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="12" r="10" />
            <polyline points="12 6 12 12 16 14" />
          </svg>
        </button>

        {/* 3. Library / Projects */}
        <button
          type="button"
          className={clsx(css.dockBtn, activeTab === 'library' && css.active)}
          title="工作区与项目"
          onClick={() => onSelectTab?.('library')}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H20v20H6.5a2.5 2.5 0 0 1-2.5-2.5Z" />
            <path d="M6 6h10" />
            <path d="M6 10h10" />
          </svg>
        </button>

        {/* 4. Mentions */}
        <button
          type="button"
          className={clsx(css.dockBtn, activeTab === 'mention' && css.active)}
          title="提及与上下文 (@)"
          onClick={() => onSelectTab?.('mention')}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="12" r="4" />
            <path d="M16 8v5a3 3 0 0 0 6 0v-1a10 10 0 1 0-4 8" />
          </svg>
        </button>

        {/* 5. More */}
        <button
          type="button"
          className={css.dockBtn}
          title="更多功能"
          onClick={onOpenSettings}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <circle cx="12" cy="12" r="1" />
            <circle cx="19" cy="12" r="1" />
            <circle cx="5" cy="12" r="1" />
          </svg>
        </button>
      </div>

      <div className={css.bottomGroup}>
        {/* Settings / Sliders */}
        <button
          type="button"
          className={css.dockBtn}
          title="设置与首选项"
          onClick={onOpenSettings}
        >
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <line x1="4" x2="4" y1="21" y2="14" />
            <line x1="4" x2="4" y1="10" y2="3" />
            <line x1="12" x2="12" y1="21" y2="12" />
            <line x1="12" x2="12" y1="8" y2="3" />
            <line x1="20" x2="20" y1="21" y2="16" />
            <line x1="20" x2="20" y1="12" y2="3" />
            <line x1="1" x2="7" y1="14" y2="14" />
            <line x1="9" x2="15" y1="8" y2="8" />
            <line x1="17" x2="23" y1="16" y2="16" />
          </svg>
        </button>

        {/* User Avatar Circle */}
        <div className={css.avatarWrapper}>
          <button
            type="button"
            className={css.avatarBtn}
            title={`用户: ${userName}`}
            onClick={() => setProfileOpen(!profileOpen)}
          >
            <span>{userName}</span>
          </button>

          {profileOpen && (
            <div className={css.profileMenu}>
              <div className={css.profileHeader}>
                <strong>Composer</strong>
                <span className={css.profileStatus}>本地工作区</span>
              </div>
              <div className={css.profileDivider} />
              <button
                type="button"
                className={css.profileItem}
                onClick={() => {
                  setProfileOpen(false)
                  onToggleTheme?.()
                }}
              >
                <span>{dark ? '切换为亮色模式 ☀️' : '切换为暗色模式 🌙'}</span>
              </button>
              <button
                type="button"
                className={css.profileItem}
                onClick={() => {
                  setProfileOpen(false)
                  onOpenSettings?.()
                }}
              >
                <span>模型与引擎设置</span>
              </button>
            </div>
          )}
        </div>
      </div>
    </aside>
  )
}
