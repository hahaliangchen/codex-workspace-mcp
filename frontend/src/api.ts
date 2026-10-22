import type {AttachmentRef} from './components/Attachments.tsx'
export interface SkillPackage {
  remote?: boolean
  origin: 'builtin' | 'external'; inject: string[]
  id: string; name: string; title: string; description: string; enabled: boolean; scope: number | null; tools: string[]
  pages: { id: string; title: string; entry: string; renderer: string }[]
}

export type PermissionMode = 'read_only' | 'workspace_write' | 'ask' | 'full_access'

export interface RequestContextMeta {
  readonly id: number
  readonly actor: string
  readonly stage: string
  readonly turn: number
  readonly step: number
  readonly nodeId: string
  readonly task_node_id?: string
  readonly workId?: string
  readonly request_id?: number
  readonly revision?: number
  readonly plan_revision?: number
  readonly review_id?: string
  readonly source_event_id?: string
  readonly model: string
  readonly created_at: number
  readonly status: string
  readonly elapsed_ms?: number
  readonly request_bytes: number
  readonly message_count: number
  readonly tools: readonly string[]
  readonly compaction?: {
    readonly raw_message_count: number
    readonly retained_raw_message_count: number
    readonly omitted_raw_message_count: number
    readonly omitted_raw_steps: readonly number[]
    readonly shortened_messages: readonly Record<string, unknown>[]
    readonly limits: Record<string, number>
    readonly request_sizes?: { readonly message_chars: number; readonly system_chars: number; readonly tool_schema_chars: number; readonly work_state_chars: number; readonly tool_count: number }
    readonly work_projection?: { readonly scope: string; readonly matched_records: number; readonly all_records_available_with: string }
  }
  readonly work?: { readonly tool_calls: number; readonly successful_file_writes: number; readonly repeated_read_calls: number }
  readonly permission_mode?: PermissionMode
  readonly final_step?: boolean
  readonly outcome?: Record<string, unknown>
}
export interface RequestContext {
  readonly metadata: RequestContextMeta
  readonly body: { readonly messages: readonly Record<string, unknown>[]; readonly tools?: readonly unknown[]; readonly [key: string]: unknown }
}
export interface ChangedFile {
  readonly path: string
  readonly added: number
  readonly deleted: number
  readonly binary: boolean
  readonly coarse: boolean
  readonly unavailable: boolean
  readonly conflicted: boolean
  readonly reverted: boolean
  readonly created: boolean
  readonly removed: boolean
}
export interface ChangesSummary {
  readonly turn: number
  readonly total: number
  readonly added: number
  readonly deleted: number
  readonly files: readonly ChangedFile[]
  readonly reverted: boolean
  readonly undo_available: boolean
  readonly complete: boolean
}
export interface FileDiff {
  readonly path: string
  readonly oldText: string | null
  readonly newText: string | null
  readonly binary: boolean
  readonly unavailable: boolean
}

export type TaskStatus =
  | 'draft' | 'running' | 'completed' | 'failed' | 'cancelled' | 'cancelling' | 'max_steps' | 'interrupted'

export interface TaskDefinition { readonly objective: string; readonly constraints: readonly string[]; readonly completion_conditions: readonly string[]; readonly version?: number }
export interface TaskBoard { readonly definition: TaskDefinition | null; readonly pending_definition: { readonly version: number; readonly definition: TaskDefinition } | null }
export interface Task {
  readonly task_board?: TaskBoard | null
  readonly task_id: string
  readonly prompt: string
  readonly kind?: 'chat' | 'code'
  readonly title?: string | null
  readonly pinned?: boolean
  readonly archived?: boolean
  readonly workspace?: string | null
  readonly model: string
  readonly provider_name?: string | null
  readonly reasoning_effort?: string | null
  readonly fast_mode?: boolean
  readonly status: TaskStatus
  readonly created_at: number
  readonly updated_at: number
  readonly parent_task_id: string | null
  /** Set for plugin page chats: pure conversations with no working directory. */
  readonly plugin_id?: string | null
  /** Paused on an "ask before running" card. */
  readonly awaiting_approval?: boolean | undefined
}

export interface Project { readonly path: string; readonly title: string }

export interface AgentInfo {
  readonly ready: boolean
  readonly workspace: string
  readonly model: string
}

export type ProviderApiType = 'openai-completions' | 'openai-responses' | 'anthropic-messages'

export const REASONING_LEVELS = ['none', 'low', 'medium', 'high', 'xhigh', 'max'] as const

export interface ModelCapabilities {
  readonly tool_calling?: boolean | null
  readonly image_input?: 'supported' | 'unsupported' | 'unknown'
  readonly image_probe?: {
    readonly status: string
    readonly capability: string
    readonly elapsed_ms: number
    readonly checked_at: number
    readonly response_body: string | null
    readonly error: string | null
  }
  readonly reasoning_efforts: readonly string[]
  readonly default_effort: string | null
  readonly fast_mode: boolean
}

export interface ProviderConfig {
  readonly prefix_system_messages?: boolean
  readonly id: string
  readonly display_name?: string | null
  readonly url: string
  readonly api_type: ProviderApiType
  readonly models: readonly string[]
  readonly model_capabilities: Readonly<Record<string, ModelCapabilities>>
  readonly has_api_key?: boolean
  readonly api_key_env?: string | null
}

export interface ModelUseSettings {
  readonly provider?: string | null
  readonly model?: string | null
  readonly reasoning_effort?: string | null
  readonly context_window?: number | null
}
export interface ModelRef { readonly provider:string; readonly model:string }

export interface CatalogModel {
  readonly provider: string
  readonly id: string
  readonly name: string
  readonly provider_name: string
  readonly enabled: boolean
  readonly available_for_agent: boolean
  readonly api_type: ProviderApiType
  readonly context_window: number | null
  readonly max_output_tokens: number | null
  readonly capabilities: Omit<ModelCapabilities, 'image_probe'> & { readonly tool_calling: boolean | null }
  readonly verification: { readonly image_input: string; readonly checked_at: number | null }
}
export interface ModelCatalog {
  readonly version: number
  readonly models: readonly CatalogModel[]
  readonly defaults: Readonly<Record<'agent' | 'visual_tool' | 'context_agent' | 'delegate_agent' | 'observer', ModelUseSettings>>
}

export interface SettingsData {
  readonly plugin_models?: Readonly<Record<string,readonly ModelRef[]>>
  readonly visual_tool?: ModelUseSettings
  readonly context_agent?: ModelUseSettings
  readonly delegate_agent?: ModelUseSettings
  readonly revision: string
  readonly providers: readonly ProviderConfig[]
  readonly orchestrator_provider: string | null
  readonly orchestrator_model: string | null
  readonly reasoning_effort: string | null
  readonly fast_mode: boolean
  readonly observer_enabled: boolean
  readonly observer_provider: string | null
  readonly observer_model: string | null
}

export interface SaveProviderInput {
  readonly prefix_system_messages?: boolean
  readonly id: string
  readonly display_name?: string | null
  readonly url: string
  readonly api_type: ProviderApiType
  readonly models: readonly string[]
  readonly model_capabilities: Readonly<Record<string, ModelCapabilities>>
  readonly api_key?: string
  readonly clear_api_key?: boolean
}

export interface SaveSettingsInput {
  readonly plugin_models?: Readonly<Record<string,readonly ModelRef[]>>
  readonly visual_tool?: ModelUseSettings
  readonly context_agent?: ModelUseSettings
  readonly delegate_agent?: ModelUseSettings
  readonly revision: string
  readonly providers: readonly SaveProviderInput[]
  readonly orchestrator_provider: string | null
  readonly orchestrator_model: string | null
  readonly reasoning_effort: string
  readonly fast_mode: boolean
  readonly observer_enabled: boolean
  readonly observer_provider: string | null
  readonly observer_model: string | null
}

export interface PluginNode {
  readonly scope: number
  readonly parent: number | null
  readonly id: string
  readonly status: string
  readonly provides: readonly string[]
  readonly requires: readonly { readonly service: string; readonly provider_scope: number }[]
}

export interface PluginEvent {
  readonly event: string
  readonly data?: { readonly id?: string }
}

export interface PluginsSnapshot {
  readonly plugins: readonly PluginNode[]
  readonly events: readonly PluginEvent[]
}

/** Persisted DSH-shaped SessionEvent as stored by the Rust task service. */
export interface AgentEvent {
  readonly seq: number
  readonly time: number
  readonly type: string
  readonly data: Record<string, unknown>
  readonly surfaceOp?: string
}

const TERMINAL: ReadonlySet<TaskStatus> = new Set(['completed', 'failed', 'cancelled', 'max_steps', 'interrupted'])

export function isTerminal(status: TaskStatus): boolean {
  return TERMINAL.has(status)
}

/** A task accepts a prompt when it has not started yet or its last turn has ended. */
export function acceptsPrompt(task: Task): boolean {
  return task.parent_task_id === null && (task.status === 'draft' || isTerminal(task.status))
}

export type MemoryKind = 'work' | 'architecture'
/** What the agent recalls by search: work lessons and its picture of the code. */
export interface Memories {
  readonly work: readonly { id: number; time: number; kind: string; summary: string; implementation: string; tests: string; risks: string; files: string[]; applies_to: string }[]
  readonly architecture: readonly { id: number; time: number; area: string; summary: string; boundaries: string; risks: string; files: string[] }[]
}

/** Selected task-flow steps sent to be distilled into a project skill. */
export interface SkillDraftInput {
  readonly goal: string
  readonly steps: readonly {
    readonly title: string
    readonly status: string
    readonly description: string
    readonly tools: readonly { readonly name: string; readonly input: string; readonly output: string; readonly failed: boolean }[]
  }[]
}

/** An editable SKILL.md draft; saved under the project's .agents/skills. */
export interface SkillDraft {
  readonly name: string
  readonly description: string
  readonly body: string
}

/** The project's AGENTS.md, injected into every request of its code sessions. */
export interface ProjectNotes {
  readonly path: string
  readonly content: string
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, init)
  const text = await response.text()
  const body: unknown = text === '' ? {} : JSON.parse(text)
  if (!response.ok) {
    const message = typeof body === 'object' && body !== null && 'error' in body ? String(body.error) : response.statusText
    throw new Error(message)
  }
  return body as T
}

function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  })
}

function put<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  })
}

function patch<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, {
    method: 'PATCH',
    headers: { 'Content-Type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  })
}

const task = (id: string): string => `/agent/tasks/${encodeURIComponent(id)}`

export const api = {
  decideTaskDefinition: (id: string, version: number, approve: boolean) => post(`${task(id)}/definition/decision`, {version, approve}),
  models: () => request<ModelCatalog>('/agent/models'),
  upload: (id:string,file:File,signal:AbortSignal,onProgress:(percent:number)=>void) => new Promise<{receiptId:string;file:AttachmentRef}>((resolve,reject)=>{
    const xhr=new XMLHttpRequest();xhr.open('POST',`/api/session/uploadFileBinary?sessionId=${encodeURIComponent(id)}&name=${encodeURIComponent(file.name)}`);xhr.setRequestHeader('Content-Type','application/octet-stream');
    const abort=()=>xhr.abort();signal.addEventListener('abort',abort,{once:true});
    xhr.upload.onprogress=e=>{if(e.lengthComputable)onProgress(Math.round(e.loaded/e.total*100))};
    xhr.onload=()=>{signal.removeEventListener('abort',abort);try{const r=JSON.parse(xhr.responseText);if(xhr.status>=400||!r.ok)throw Error(r.error?.message||'上传失败');resolve(r.value)}catch(e){reject(e)}};
    xhr.onerror=()=>{signal.removeEventListener('abort',abort);reject(Error('附件上传失败'))};xhr.onabort=()=>{signal.removeEventListener('abort',abort);reject(Error('上传已取消'))};
    if(signal.aborted){signal.removeEventListener('abort',abort);reject(Error('上传已取消'));return}xhr.send(file)
  }),
  memories: (workspace: string) => request<Memories>(`/agent/memories?workspace=${encodeURIComponent(workspace)}`),
  updateMemory: (workspace: string, kind: MemoryKind, id: number, summary: string) => patch<Memories>(`/agent/memories/${kind}/${id}`, { workspace, summary }),
  forgetMemory: (workspace: string, kind: MemoryKind, id: number) => request<Memories>(`/agent/memories/${kind}/${id}?workspace=${encodeURIComponent(workspace)}`, { method: 'DELETE' }),
  searchSessions: (q: string) => request<{ matches: { task_id: string; snippet: string }[] }>(`/agent/sessions/search?q=${encodeURIComponent(q)}`),
  rewind: (taskId: string, turn: number) => post<{ turn: number; prompt: string; undone_turns: number[] }>(`${task(taskId)}/rewind`, { turn }),
  interject: (taskId: string, text: string) => post<{ seq: number }>(`${task(taskId)}/interjections`, { text }),
  skillDraft: (taskId: string, input: SkillDraftInput) => post<SkillDraft>(`${task(taskId)}/skill-draft`, input),
  saveSkill: (taskId: string, draft: SkillDraft, overwrite = false) => post<{ name: string; path: string }>(`${task(taskId)}/skills`, { ...draft, overwrite }),
  notes: (workspace: string) => request<ProjectNotes>(`/agent/notes?workspace=${encodeURIComponent(workspace)}`),
  saveNotes: (workspace: string, content: string) => put<ProjectNotes>('/agent/notes', { workspace, content }),
  addNote: (workspace: string, note: string) => post<ProjectNotes>('/agent/notes', { workspace, note }),
  skillPackages: () => request<{ packages: SkillPackage[] }>('/agent/skill-packages'),
  installPackage: (path: string) => post<{ packages: SkillPackage[] }>('/agent/skill-packages', { path }),
  connectRemotePackage: (url: string, token: string) => post<{ packages: SkillPackage[] }>('/agent/skill-packages/remote', { url, token }),
  pluginSession: <T>(id: string, payload: unknown) => post<T>(`/agent/skill-packages/${encodeURIComponent(id)}/session`, payload),
  pluginService: <T>(id: string, payload: unknown) => post<T>(`/agent/skill-packages/${encodeURIComponent(id)}/service`, payload),
  reloadPackage: (id: string) => post<{ packages: SkillPackage[] }>(`/agent/skill-packages/${encodeURIComponent(id)}/reload`, {}),
  removePackage: (id: string) => request<{ packages: SkillPackage[] }>(`/agent/skill-packages/${encodeURIComponent(id)}`, { method: 'DELETE' }),
  setPackageEnabled: (id: string, enabled: boolean) => post<{ packages: SkillPackage[] }>(`/agent/skill-packages/${encodeURIComponent(id)}/enabled`, { enabled }),
  skillTool: <T>(id: string, name: string, payload: unknown) => post<T>(`/agent/skill-packages/${encodeURIComponent(id)}/tools/${encodeURIComponent(name)}`, payload),
  info: () => request<AgentInfo>('/agent/info'),
  listTasks: async (limit = 50) => (await request<{ tasks: Task[] }>(`/agent/tasks?limit=${limit}`)).tasks,
  projects: async () => (await request<{projects: Project[]}>('/agent/workspaces')).projects,
  addProject: (path: string) => post<Project>('/agent/workspaces', {path}),
  renameProject: (path: string, title: string) => patch('/agent/workspaces', {path, title}),
  removeProject: (path: string) => request('/agent/workspaces', {method: 'DELETE', headers: {'Content-Type':'application/json'}, body: JSON.stringify({path})}),
  updateSession: (id: string, payload: {title?: string; pinned?: boolean; archived?: boolean}) => patch(`/agent/sessions/${encodeURIComponent(id)}`, payload),
  deleteSession: (id: string) => request<{deleted: string[]}>(`/agent/sessions/${encodeURIComponent(id)}`, {method: 'DELETE'}),
  getTask: (id: string) => request<Task>(task(id)),
  updateTask: (id: string, payload: { model?: string | undefined; provider?: string | undefined; reasoning_effort?: string | null | undefined; fast_mode?: boolean | undefined }) =>
    patch<Task>(task(id), payload),
  events: async (id: string) => (await request<{ events: AgentEvent[] }>(`${task(id)}/events`)).events,
  contextDebug: (id: string) => request<{ enabled: boolean }>(`${task(id)}/context-debug`, { cache: 'no-store' }),
  setContextDebug: (id: string, enabled: boolean) => put<{ enabled: boolean }>(`${task(id)}/context-debug`, { enabled }),
  requestContexts: (id: string, before?: number) => request<{ items: readonly RequestContextMeta[]; has_more: boolean }>(`${task(id)}/request-contexts${before === undefined ? '' : `?before=${before}`}`, { cache: 'no-store' }),
  requestContext: (id: string, requestId: number) => request<RequestContext>(`${task(id)}/request-contexts/${requestId}`, { cache: 'no-store' }),
  changes: (id: string, turn: number) => request<ChangesSummary>(`${task(id)}/changes/${turn}`),
  changeDiff: (id: string, turn: number, index: number, path?: string) => request<FileDiff>(`${task(id)}/changes/${turn}/files/${index}${path === undefined ? '' : `?path=${encodeURIComponent(path)}`}`, { cache: 'no-store' }),
  undoChanges: (id: string, turn: number) => post<ChangesSummary>(`${task(id)}/changes/${turn}/undo`, {}),
  createSession: async (workspace?: string, kind: 'chat' | 'code' = 'code') => (await post<{ task_id: string }>('/agent/sessions', {workspace, kind})).task_id,
  prompt: (id: string, prompt: string, options?: { permission_mode?: PermissionMode | undefined; model?: string | undefined; provider?: string | undefined; reasoning_effort?: string | undefined; fast_mode?: boolean | undefined; content?: readonly unknown[]; requestId?: string }) =>
    post<{ task_id: string }>(`${task(id)}/messages`, { prompt, ...options }),
  cancel: (id: string) => request<unknown>(task(id), { method: 'DELETE' }),
  interruptNode: (id: string, nodeId: string) =>
    post<unknown>(`${task(id)}/flow/nodes/${encodeURIComponent(nodeId)}/interrupt`),
  getSettings: () => request<SettingsData>('/agent/settings/data'),
  saveSettings: (payload: SaveSettingsInput) => put<SettingsData>('/agent/settings/data', payload),
  probeImage: (payload: { revision: string; provider: string; model: string }) => post<{ probe: Record<string, unknown>; settings: SettingsData }>('/agent/settings/probe-image', payload),
  discoverModels: (input: { id: string; url: string; api_key?: string }) =>
    post<{ models: string[] }>('/agent/settings/discover', input),
  getPlugins: () => request<PluginsSnapshot>('/agent/plugins'),
  files: (query: string) => request<{ files: readonly { path: string; kind: 'file' | 'dir' }[] }>(`/agent/workspace/files?q=${encodeURIComponent(query)}`),
  commands: () => request<{ commands: readonly { name: string; description: string }[] }>('/agent/commands'),
  /** Subscribe to events after `since`; the server replays anything newer first. */
  follow(id: string, since: number, handlers: {
    onEvent: (event: AgentEvent) => void
    onOpen: () => void
    onError: () => void
  }): () => void {
    const source = new EventSource(`${task(id)}/stream?since=${since}`)
    source.addEventListener('session/event', (message) => {
      handlers.onEvent(JSON.parse((message as MessageEvent<string>).data) as AgentEvent)
    })
    source.onopen = handlers.onOpen
    source.onerror = handlers.onError
    return () => { source.close() }
  },
}
