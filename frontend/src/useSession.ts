import { useCallback, useEffect, useRef, useState } from 'react'
import { type AgentEvent, type Task } from './api.ts'
import { useAgentApi } from './cordis/react.tsx'

export type Connection = 'idle' | 'connecting' | 'connected' | 'reconnecting'

export interface Session {
  readonly task: Task | undefined
  readonly events: readonly AgentEvent[]
  readonly connection: Connection
  readonly error: string | undefined
  /** Re-read the task and follow its stream again, e.g. after sending a prompt. */
  readonly reload: () => void
}

const live = (task: Task): boolean => task.status === 'running' || task.status === 'cancelling'

/**
 * The Rust stream replays events after `since` and ends once the task stops
 * running. A terminal event can precede the task status update, so wait for
 * the status before closing EventSource.
 */
export function useSession(taskId: string | undefined, onSettled: () => void): Session {
  const api = useAgentApi()
  const [task, setTask] = useState<Task>()
  const [events, setEvents] = useState<readonly AgentEvent[]>([])
  const [connection, setConnection] = useState<Connection>('idle')
  const [error, setError] = useState<string>()
  const generation = useRef(0)
  const lastSeq = useRef(-1)
  const close = useRef<() => void>(() => {})
  const settled = useRef(onSettled)
  settled.current = onSettled

  const load = useCallback(async (id: string, run: number) => {
    const stale = () => run !== generation.current
    close.current()
    close.current = () => {}
    try {
      const [nextTask, history] = await Promise.all([api.getTask(id), api.events(id)])
      if (stale()) return
      setTask(nextTask)
      setError(undefined)
      setEvents(history)
      lastSeq.current = history.at(-1)?.seq ?? -1
      if (!live(nextTask)) {
        setConnection('idle')
        return
      }
      setConnection('connecting')
      let settling = false
      const pending: AgentEvent[] = []
      let flushTimer: number | undefined
      const discardPending = () => {
        if (flushTimer !== undefined) window.clearTimeout(flushTimer)
        flushTimer = undefined
        pending.length = 0
      }
      const flush = () => {
        flushTimer = undefined
        if (stale()) {
          pending.length = 0
          return
        }
        const batch = pending.splice(0)
        if (batch.length > 0) setEvents(current => [...current, ...batch])
      }
      const stop = api.follow(id, lastSeq.current, {
        onOpen: () => { if (!stale()) setConnection('connected') },
        onEvent: (event) => {
          if (stale() || event.seq <= lastSeq.current) return
          lastSeq.current = event.seq
          pending.push(event)
          // One React update per burst, while message components animate
          // independently. Preserve every event and its ordering in history.
          if (flushTimer === undefined) flushTimer = window.setTimeout(flush, 40)
        },
        onError: () => {
          if (stale() || settling) return
          settling = true
          setConnection('reconnecting')
          void api.getTask(id).then((current) => {
            if (stale()) return
            setTask(current)
            if (live(current)) {
              settling = false
              return
            }
            // The connection may have dropped before the final events arrived.
            // Refresh the durable history before settling this task.
            void api.events(id).then((history) => {
              if (stale()) return
              discardPending()
              setEvents(history)
              lastSeq.current = history.at(-1)?.seq ?? -1
              stop()
              setConnection('idle')
              settled.current()
            }, () => {
              settling = false
              if (!stale()) setConnection('reconnecting')
            })
          }, () => {
            settling = false
            if (!stale()) setConnection('reconnecting')
          })
        },
      })
      close.current = () => {
        stop()
        discardPending()
      }
    } catch (cause) {
      if (!stale()) setError(cause instanceof Error ? cause.message : String(cause))
    }
  }, [api])

  useEffect(() => {
    generation.current += 1
    const run = generation.current
    setTask(undefined)
    setEvents([])
    setError(undefined)
    setConnection('idle')
    lastSeq.current = -1
    if (taskId !== undefined) void load(taskId, run)
    return () => {
      generation.current += 1
      close.current()
      close.current = () => {}
    }
  }, [taskId, load])

  // The Observer has its own completion path after the Worker finishes. Keep
  // reading its durable events briefly after the Worker stream closes.
  useEffect(() => {
    if (!taskId || !task || live(task)) return
    if (!['completed', 'failed', 'max_steps'].includes(task.status)) return
    if (!events.some(event => event.type === 'observer/config' && event.data.enabled === true)) return
    if (events.some(event => event.type === 'observer/retrospective')) return
    const run = generation.current
    const deadline = Date.now() + 65_000
    const timer = window.setInterval(() => {
      if (run !== generation.current || Date.now() >= deadline) {
        window.clearInterval(timer)
        return
      }
      void api.events(taskId).then(history => {
        if (run !== generation.current) return
        if ((history.at(-1)?.seq ?? -1) > lastSeq.current) {
          lastSeq.current = history.at(-1)?.seq ?? -1
          setEvents(history)
        }
      }, () => {})
    }, 1500)
    return () => window.clearInterval(timer)
  }, [api, taskId, task, events])

  const reload = useCallback(() => {
    if (taskId === undefined) return
    generation.current += 1
    void load(taskId, generation.current)
  }, [taskId, load])

  return { task, events, connection, error, reload }
}
