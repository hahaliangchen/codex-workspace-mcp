import { zh as chat } from './dsh/locale/chat.ts'
import { zh as common } from './dsh/locale/common.ts'
import { zh as conversation } from './dsh/locale/conversation.ts'
import type { Translate } from './dsh/contract/slots.ts'

const app = {
  'app.title': 'DeepSeek Harness',
  'app.settings': '设置',
  'app.theme': '切换外观',
  'app.chat': '对话',
  'app.flow': '任务流',
  'app.trajectory': '轨迹',
  'app.subagent': '子代理',
  'app.backToParent': '返回主会话',
  'app.new': '新对话',
  'app.untitled': '新会话',
  'app.noSessions': '暂无会话',
  'app.waiting': '等待事件返回…',
  'app.workspace': '工作区',
  'app.searchPlaceholder': '搜索会话…',
  'app.connecting': '连接中',
  'app.connected': '已连接',
  'app.reconnecting': '重新连接中',
  'app.notReady': '尚未配置主代理模型，请点击右下角模型选择器或左侧「设置」配置模型。',
  'app.childReadonly': '当前为子代理只读会话，返回主会话可继续发送消息。',
  'app.running': '任务正在运行…',
  'app.viewSubagent': '查看子代理轨迹',
  'app.subagentStarted': '已派生子代理',
  'app.turn': '第 {turn} 轮',
  'app.step': '第 {turn} 轮 · 步骤 {step}',
  'app.completed': '已完成',
  'app.aborted': '已停止',
  'app.failed': '执行失败',
  'app.processSummary': '已执行 {count} 个步骤',
  'app.processRunning': '正在执行步骤 ({count})…',
  'app.configureModel': '选择模型',
  'sidebar.collapse': '收起侧边栏',
  'sidebar.expand': '展开侧边栏',
  'status.draft': '新会话',
  'status.running': '运行中',
  'status.completed': '已完成',
  'status.failed': '失败',
  'status.cancelled': '已取消',
  'status.cancelling': '正在停止',
  'status.max_steps': '已达步数上限',
  'status.interrupted': '已中断',
  'trajectory.turnStart': '第 {turn} 轮开始',
  'trajectory.userMessage': '用户消息',
  'trajectory.stepStart': '步骤 {step} 请求',
  'trajectory.assistant': '模型回复',
  'trajectory.toolCall': '{name}',
  'trajectory.toolResult': '工具结果',
  'trajectory.stepEnd': '步骤 {step} 完成',
  'trajectory.turnEnd': '第 {turn} 轮结束',
  'trajectory.subagentStart': '子代理启动',
  'trajectory.subagentEnd': '子代理完成',
  'placeholder.default': '随心输入',
  'placeholder.hero': '随心输入',
} as const

const dictionary: Readonly<Record<string, string>> = { ...common, ...conversation, ...chat, ...app }

export const t: Translate = (key, params) => {
  const template = dictionary[key] ?? key
  if (params === undefined) return template
  return template.replace(/\{(\w+)\}/g, (match, name: string) => {
    const value = params[name]
    return value === undefined ? match : String(value)
  })
}
