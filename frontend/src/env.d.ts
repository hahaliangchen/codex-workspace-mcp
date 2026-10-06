/// <reference types="vite/client" />

declare const process: {
  readonly env: {
    readonly NODE_ENV?: string
  }
}

declare module '*.module.css' {
  const classes: Record<string, string>
  export default classes
}

declare module '*.css'
declare module '*.png' {
  const src: string
  export default src
}
