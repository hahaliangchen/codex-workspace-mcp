import { useState, useEffect, useMemo } from 'react'
import type { AgentEvent, Task } from './api.ts'
import { getTaskDurationInfo, type TaskDurationInfo } from './model.ts'

/**
 * React hook that returns live or final timing metrics for the current task/turn.
 * When running is true, it ticks once per second so the elapsed duration updates in real time.
 */
export function useTaskDuration(
  events: readonly AgentEvent[],
  task?: Task,
  running?: boolean
): TaskDurationInfo {
  const [now, setNow] = useState(() => Date.now())

  const isRunning = Boolean(
    running ||
    task?.status === 'running' ||
    task?.status === 'cancelling'
  )

  useEffect(() => {
    if (!isRunning) return
    setNow(Date.now())
    const timer = setInterval(() => {
      setNow(Date.now())
    }, 1000)
    return () => clearInterval(timer)
  }, [isRunning])

  return useMemo(
    () => getTaskDurationInfo(events, task, isRunning, now),
    [events, task, isRunning, now]
  )
}
