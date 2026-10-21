import type { Task, Project } from './api.ts'

export function workspaceName(path: string): string {
  return path.replace(/^\\\\\?\\/, '').split(/[\\/]/).filter(Boolean).at(-1) ?? path
}
export function taskTitle(task: Task): string {
  return task.title?.trim() || task.prompt.trim().split('\n', 1)[0] || '新会话'
}
export const isChat = (task: Task): boolean => !!task.plugin_id || task.kind === 'chat'
/** `contentHits`: sessions whose messages (not only titles) match the query. */
export function visibleTasks(tasks: readonly Task[], query: string, archived: boolean, contentHits?: ReadonlySet<string>): Task[] {
  const q = query.trim().toLocaleLowerCase()
  return tasks.filter(task => (!!task.archived === archived)
    && (!!task.prompt.trim() || task.status === 'running' || task.status === 'cancelling')
    && (!q || taskTitle(task).toLocaleLowerCase().includes(q) || !!contentHits?.has(task.task_id)))
    .sort((a, b) => Number(!!b.pinned) - Number(!!a.pinned) || b.updated_at - a.updated_at || a.task_id.localeCompare(b.task_id))
}
export function projectGroups(projects: readonly Project[], tasks: readonly Task[], defaultWorkspace: string) {
  // Registered order is stable; selection never changes it. Identity is the full
  // canonical path, so two real folders with the same basename stay distinct.
  const unique = [...new Map(projects.map(project => [project.path, project])).values()]
  const groups = unique.map(project => ({...project, tasks: tasks.filter(task => !isChat(task) && (task.workspace ?? defaultWorkspace) === project.path)}))
  const paths = new Set(unique.map(project => project.path))
  const ungrouped = tasks.filter(task => !isChat(task) && !paths.has(task.workspace ?? defaultWorkspace))
  return {groups, ungrouped}
}
