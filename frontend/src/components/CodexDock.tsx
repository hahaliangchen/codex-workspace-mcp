import clsx from 'clsx'
import css from './CodexDock.module.css'

export type DockTab = 'home' | 'skills'

interface CodexDockProps {
  activeTab?: DockTab
  onSelectTab?: (tab: DockTab) => void
  onNewChat?: () => void
  onOpenSettings?: () => void
  onToggleTheme?: () => void
  dark?: boolean
}

export function CodexDock({
  activeTab = 'home',
  onSelectTab,
  onNewChat,
  onOpenSettings,
  onToggleTheme,
  dark,
}: CodexDockProps) {
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

        <button type="button" className={clsx(css.dockBtn, activeTab === 'skills' && css.active)} title="技能页面" onClick={() => onSelectTab?.('skills')}>
          <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2"><rect x="3" y="3" width="18" height="18" rx="3" /><path d="M3 9h18M9 9v12" /></svg>
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

        <button
          type="button"
          className={css.dockBtn}
          title={dark ? '切换为浅色' : '切换为深色'}
          aria-label={dark ? '切换为浅色' : '切换为深色'}
          onClick={onToggleTheme}
        >
          {dark
            ? <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round"><circle cx="12" cy="12" r="4" /><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" /></svg>
            : <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinejoin="round"><path d="M20 14.5A8 8 0 0 1 9.5 4a8 8 0 1 0 10.5 10.5Z" /></svg>}
        </button>
      </div>
    </aside>
  )
}
