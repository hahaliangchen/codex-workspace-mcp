import { useEffect, useRef } from 'react'
import type { Task } from './api.ts'
import { taskTitle } from './sidebarModel.ts'
import { brandIcon } from './theme.ts'

type Watched = { readonly running: boolean; readonly awaiting: boolean }

const ENDINGS: Partial<Record<Task['status'], string>> = {
  completed: '任务完成了', failed: '任务出错了', max_steps: '任务达到步数上限', interrupted: '任务中断了', cancelled: '任务已停止',
}

/** Ask once, from a user gesture: browsers ignore permission prompts made without one. */
export function requestNotificationPermission(): void {
  try {
    if ('Notification' in window && Notification.permission === 'default') void Notification.requestPermission()
  } catch { /* unsupported: notifications stay off */ }
}

/** Two soft sine notes; quiet enough to notice without startling. */
function chime(): void {
  try {
    const audio = new AudioContext()
    ;[[880, 0], [1175, 0.14]].forEach(([frequency, delay]) => {
      const tone = audio.createOscillator()
      const gain = audio.createGain()
      tone.type = 'sine'
      tone.frequency.value = frequency!
      const start = audio.currentTime + delay!
      gain.gain.setValueAtTime(0.0001, start)
      gain.gain.exponentialRampToValueAtTime(0.08, start + 0.02)
      gain.gain.exponentialRampToValueAtTime(0.0001, start + 0.35)
      tone.connect(gain).connect(audio.destination)
      tone.start(start)
      tone.stop(start + 0.4)
    })
    window.setTimeout(() => { void audio.close() }, 1000)
  } catch { /* no audio output */ }
}

function notify(title: string, body: string, onClick: () => void): void {
  chime()
  try {
    if (!('Notification' in window) || Notification.permission !== 'granted') return
    const note = new Notification(title, { body, icon: brandIcon(), tag: title + body })
    note.onclick = () => { window.focus(); onClick(); note.close() }
  } catch { /* notifications unavailable */ }
}

/**
 * Tell the user when a task they are not looking at finishes or stops to ask
 * for approval. The task list refreshes every few seconds, which covers every
 * session, not only the one on screen.
 */
export function useTaskNotifications(tasks: readonly Task[], currentTaskId: string | undefined, onOpen: (taskId: string) => void): void {
  const seen = useRef<Map<string, Watched> | null>(null)
  const open = useRef(onOpen)
  open.current = onOpen
  const current = useRef(currentTaskId)
  current.current = currentTaskId
  useEffect(() => {
    const next = new Map(tasks.map(task => [task.task_id, {
      running: task.status === 'running' || task.status === 'cancelling',
      awaiting: task.awaiting_approval === true,
    }]))
    const previous = seen.current
    seen.current = next
    // The first list only establishes what was already true.
    if (previous === null) return
    const hidden = document.hidden || !document.hasFocus()
    for (const task of tasks) {
      // Whatever happens in the session on screen is already visible there.
      const away = hidden || task.task_id !== current.current
      const before = previous.get(task.task_id)
      const now = next.get(task.task_id)!
      if (!before) continue
      const title = taskTitle(task)
      if (away && !before.awaiting && now.awaiting) {
        notify('需要你确认一下', `「${title}」想运行命令，正在等你同意`, () => open.current(task.task_id))
      } else if (away && before.running && !now.running && ENDINGS[task.status]) {
        notify(ENDINGS[task.status]!, title, () => open.current(task.task_id))
      }
    }
  }, [tasks])
}
