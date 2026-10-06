export type PermissionMode = 'read_only' | 'workspace_write' | 'full_access'

export interface RequestContextMeta {
  readonly id: number
  readonly actor: string
  readonly stage: string
  readonly turn: number
  readonly step: number
  readonly nodeId: string
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
export interface NotebookMaterial {
  readonly id: number
  readonly path: string
  readonly symbol: string
  readonly start_line: number
  readonly end_line: number
  readonly node_id: string
  readonly turn: number
  readonly file_hash: string
  readonly status: string
}
export interface FindingHistoryPage {
  readonly items: readonly { readonly seq: number; readonly time: number; readonly record: Record<string, unknown> }[]
  readonly has_more: boolean
  readonly next_before: number | null
}
export interface NotebookSource {
  readonly id: number
  readonly status: string
  readonly available: boolean
  readonly content?: string
  readonly start_line?: number
  readonly end_line?: number
  readonly complete?: boolean
  readonly next_start_line?: number | null
  readonly start_column?: number
  readonly next_column?: number | null
  readonly partial_line?: boolean
  readonly byte_exact?: boolean
  readonly descriptor: NotebookMaterial
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

export interface Task {
  readonly task_id: string
  readonly prompt: string
  readonly model: string
  readonly provider_name?: string | null
  readonly reasoning_effort?: string | null
  readonly fast_mode?: boolean
  readonly status: TaskStatus
  readonly created_at: number
  readonly updated_at: number
  readonly parent_task_id: string | null
}

export interface AgentInfo {
  readonly ready: boolean
  readonly workspace: string
  readonly model: string
  readonly subagent_enabled: boolean
  readonly subagent_model: string
}

export type ProviderApiType = 'openai-completions' | 'openai-responses' | 'anthropic-messages'

export const REASONING_LEVELS = ['none', 'low', 'medium', 'high', 'xhigh', 'max'] as const

export interface ModelCapabilities {
  readonly image_input?: 'supported' | 'unsupported' | 'unknown'
  readonly reasoning_efforts: readonly string[]
  readonly default_effort: string | null
  readonly fast_mode: boolean
}

export interface ProviderConfig {
  readonly id: string
  readonly display_name?: string | null
  readonly url: string
  readonly api_type: ProviderApiType
  readonly models: readonly string[]
  readonly model_capabilities: Readonly<Record<string, ModelCapabilities>>
  readonly has_api_key?: boolean
  readonly api_key_env?: string | null
}

export interface SettingsData {
  readonly visual_fallback_enabled?: boolean
  readonly visual_provider?: string | null
  readonly visual_model?: string | null
  readonly revision: string
  readonly providers: readonly ProviderConfig[]
  readonly orchestrator_provider: string | null
  readonly orchestrator_model: string | null
  readonly reasoning_effort: string | null
  readonly fast_mode: boolean
  readonly expert_provider: string | null
  readonly expert_model: string | null
  readonly enable_subagent: boolean
  readonly observer_enabled: boolean
  readonly observer_provider: string | null
  readonly observer_model: string | null
}

export interface SaveProviderInput {
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
  readonly visual_fallback_enabled?: boolean
  readonly visual_provider?: string | null
  readonly visual_model?: string | null
  readonly revision: string
  readonly providers: readonly SaveProviderInput[]
  readonly orchestrator_provider: string | null
  readonly orchestrator_model: string | null
  readonly reasoning_effort: string
  readonly fast_mode: boolean
  readonly expert_provider: string | null
  readonly expert_model: string | null
  readonly enable_subagent: boolean
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
  info: () => request<AgentInfo>('/agent/info'),
  listTasks: async (limit = 50) => (await request<{ tasks: Task[] }>(`/agent/tasks?limit=${limit}`)).tasks,
  getTask: (id: string) => request<Task>(task(id)),
  updateTask: (id: string, payload: { model?: string | undefined; provider?: string | undefined; reasoning_effort?: string | null | undefined; fast_mode?: boolean | undefined }) =>
    patch<Task>(task(id), payload),
  events: async (id: string) => (await request<{ events: AgentEvent[] }>(`${task(id)}/events`)).events,
  contextDebug: (id: string) => request<{ enabled: boolean }>(`${task(id)}/context-debug`, { cache: 'no-store' }),
  setContextDebug: (id: string, enabled: boolean) => put<{ enabled: boolean }>(`${task(id)}/context-debug`, { enabled }),
  requestContexts: (id: string, before?: number) => request<{ items: readonly RequestContextMeta[]; has_more: boolean }>(`${task(id)}/request-contexts${before === undefined ? '' : `?before=${before}`}`, { cache: 'no-store' }),
  requestContext: (id: string, requestId: number) => request<RequestContext>(`${task(id)}/request-contexts/${requestId}`, { cache: 'no-store' }),
  notebook: (id: string) => request<{ materials: readonly NotebookMaterial[]; work: Record<string, unknown> }>(`${task(id)}/notebook`, { cache: 'no-store' }),
  findingHistory: (id: string, before?: number) => request<FindingHistoryPage>(`${task(id)}/notebook/history${before === undefined ? '' : `?before=${before}`}`, { cache: 'no-store' }),
  notebookMaterial: (id: string, material: number, start?: number, column?: number) => request<NotebookSource>(`${task(id)}/notebook/materials/${material}${start === undefined ? '' : `?start_line=${start}${column === undefined ? '' : `&start_column=${column}`}`}`, { cache: 'no-store' }),
  changes: (id: string, turn: number) => request<ChangesSummary>(`${task(id)}/changes/${turn}`),
  changeDiff: (id: string, turn: number, index: number, path?: string) => request<FileDiff>(`${task(id)}/changes/${turn}/files/${index}${path === undefined ? '' : `?path=${encodeURIComponent(path)}`}`, { cache: 'no-store' }),
  undoChanges: (id: string, turn: number) => post<ChangesSummary>(`${task(id)}/changes/${turn}/undo`, {}),
  createSession: async () => (await post<{ task_id: string }>('/agent/sessions')).task_id,
  prompt: (id: string, prompt: string, options?: { permission_mode?: PermissionMode | undefined; model?: string | undefined; provider?: string | undefined; reasoning_effort?: string | undefined; fast_mode?: boolean | undefined }) =>
    post<{ task_id: string }>(`${task(id)}/messages`, { prompt, ...options }),
  cancel: (id: string) => request<unknown>(task(id), { method: 'DELETE' }),
  interruptNode: (id: string, nodeId: string) =>
    post<unknown>(`${task(id)}/flow/nodes/${encodeURIComponent(nodeId)}/interrupt`),
  getSettings: () => request<SettingsData>('/agent/settings/data'),
  saveSettings: (payload: SaveSettingsInput) => put<SettingsData>('/agent/settings/data', payload),
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
