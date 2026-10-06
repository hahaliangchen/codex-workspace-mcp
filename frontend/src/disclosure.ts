import { useCallback, useState } from 'react'
import type { UseDisclosure } from './dsh/contract/slots.ts'

/** Local open state per row; ui-chat additionally resets it when the owning Turn folds. */
export const useDisclosure: UseDisclosure = () => {
  const [expanded, setExpanded] = useState(false)
  const toggle = useCallback(() => { setExpanded(open => !open) }, [])
  return { expanded, setExpanded, toggle }
}
