import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const dsh = (path: string): string => fileURLToPath(new URL(`./src/dsh/${path}`, import.meta.url))
const backend = process.env.AGENT_BACKEND ?? 'http://127.0.0.1:3001'

export default defineConfig({
  base: '/agent/',
  plugins: [react()],
  resolve: {
    alias: {
      '@deepseek-ai/cordis': dsh('cordis/index.ts'),
      '@deepseek-ai/dsh-client-ui-primitives': dsh('ui-primitives/index.ts'),
      '@deepseek-ai/dsh-client-store': dsh('store/index.ts'),
      '@deepseek-ai/dsh-client-ui-slots': dsh('ui-slots/index.ts'),
      '@deepseek-ai/dsh-util-workspace-path': dsh('workspace-path/index.ts'),
    },
  },
  server: {
    proxy: {
      '/agent/tasks': backend,
      '/agent/sessions': backend,
      '/agent/info': backend,
      '/agent/settings': backend,
      '/agent/plugins': backend,
      '/agent/workspace/files': backend,
      '/agent/commands': backend,
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 4096,
  },
})
