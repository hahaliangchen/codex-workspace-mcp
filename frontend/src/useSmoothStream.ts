import { useEffect, useRef, useState } from 'react'

/** Keep the received text intact; only its visible prefix is animated. */
export function useSmoothStream(text: string, streaming: boolean) {
  // Settled history is immediate; a newly mounted live batch also starts
  // with a visible prefix rather than appearing as one solid block.
  const [visible, setVisible] = useState(streaming ? '' : text)
  const shown = useRef(streaming ? '' : text)
  const target = useRef(text)
  const active = useRef(streaming)
  const wasStreaming = useRef(streaming)
  const frame = useRef<number>()
  const lastTime = useRef(0)
  const credit = useRef(0)

  useEffect(() => {
    target.current = text
    active.current = streaming
    const animate = streaming || wasStreaming.current
    wasStreaming.current = streaming

    const stop = () => {
      if (frame.current !== undefined) cancelAnimationFrame(frame.current)
      frame.current = undefined
      lastTime.current = 0
      credit.current = 0
    }
    const showAll = () => {
      stop()
      shown.current = target.current
      setVisible(target.current)
    }

    // A corrected answer, a history snapshot, or a background tab must not
    // replay stale text. Completion of a live answer still drains its tail.
    if (!animate || !text.startsWith(shown.current) || document.visibilityState === 'hidden') {
      showAll()
      return
    }
    if (shown.current === text || frame.current !== undefined) return

    lastTime.current = performance.now()
    const paint = (now: number) => {
      frame.current = undefined
      const remaining = target.current.length - shown.current.length
      if (remaining <= 0) {
        lastTime.current = 0
        credit.current = 0
        return
      }
      const elapsed = Math.min(48, Math.max(0, now - lastTime.current)) / 1000
      lastTime.current = now
      // Catch up adaptively after a large batch, with only a short display
      // delay; the final tail catches up faster without appearing all at once.
      const rate = Math.max(70, remaining / (active.current ? 0.24 : 0.12))
      credit.current += rate * elapsed
      const count = Math.floor(credit.current)
      let end = Math.min(target.current.length, shown.current.length + count)
      // Never display half a UTF-16 surrogate pair (for example an emoji).
      const previous = target.current.charCodeAt(end - 1)
      if (end < target.current.length && previous >= 0xd800 && previous <= 0xdbff) end -= 1
      if (end > shown.current.length) {
        credit.current -= end - shown.current.length
        shown.current = target.current.slice(0, end)
        setVisible(shown.current)
      }
      if (shown.current !== target.current) frame.current = requestAnimationFrame(paint)
      else {
        lastTime.current = 0
        credit.current = 0
      }
    }
    frame.current = requestAnimationFrame(paint)
  }, [text, streaming])

  useEffect(() => {
    const onVisibility = () => {
      if (frame.current !== undefined) cancelAnimationFrame(frame.current)
      frame.current = undefined
      shown.current = target.current
      setVisible(target.current)
      lastTime.current = 0
      credit.current = 0
    }
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      document.removeEventListener('visibilitychange', onVisibility)
      if (frame.current !== undefined) cancelAnimationFrame(frame.current)
      frame.current = undefined
      lastTime.current = 0
      credit.current = 0
    }
  }, [])

  return { text: visible, streaming: streaming || visible !== text }
}
