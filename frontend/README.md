# Agent browser frontend

The browser runtime uses the TypeScript Cordis source copied from DeepSeek
Harness commit `46a7f68`. Source mapping and licenses are in
`src/dsh/SOURCE.md`.

## Startup

`src/main.tsx` boots Cordis, then mounts the original DSH UI renderer.
`src/cordis/runtime.ts` registers plugins in dependency order:

1. Rust task HTTP/SSE API service.
2. DSH `ui-renderer` service, backed by the copied `ui-slots` core.
3. An empty session-scope adapter required by the renderer root.
4. Page root declaring four child slots.
5. Sidebar, chat, trajectory, and composer contributions.

The root component calls the renderer's `renderSlot` for each region. Cordis
owns plugin fibers and unregisters their contributions on disposal. The page
components under `src/components/` adapt Rust task data and retain DSH styling.
`src/useAgentWorkspace.ts` owns task selection, task list refresh, parent-task
lookup, and submit/stop commands. `src/components/AgentPage.tsx` owns the hero
and conversation layouts. `src/App.tsx` only joins the task model and root
slots; `src/useSession.ts` continues to own event history and the live stream.

## Build

```sh
npm install
npm run build
```

The build writes `dist/`, which Rust serves at `/agent`. For frontend work,
`npm run dev` serves `/agent/` with task API requests proxied to port 3001.

## Scope

The Cordis core, UI slot rules, and React renderer are local source. DSH's
dynamic module loader, Remote RPC, session-scoped UI business plugins, and
third-party plugin installation are not part of this frontend composition.
