import type { IconProps } from './icons/props.ts'
import { useBrandIcon } from '../../brand.ts'

/** Native viewBox of {@link FISH_LOGO_PATH} (width and height in user units). */
export const FISH_LOGO_VIEWBOX = { width: 24, height: 24 }

export const FISH_LOGO_PATH = ''

/**
 * Render the sprout brand logo replacing the legacy fish/whale logo.
 * @param props.size - width/height in px (default 24).
 * @param props.className - extra class for layout placement.
 */
export function FishLogo({ size = 24, className }: IconProps) {
  const logo = useBrandIcon()
  return (
    <img
      src={logo}
      width={size}
      height={size}
      className={className}
      alt=""
      style={{
        objectFit: 'contain',
        display: 'inline-block',
        verticalAlign: 'middle',
        userSelect: 'none',
      }}
    />
  )
}
