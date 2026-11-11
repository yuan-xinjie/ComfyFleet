import type { DepsReport, NodeDeps } from '../types'

/**
 * 启动时自动补依赖只处理真正缺东西的启用节点。
 *
 * `DepsReport.pendingNodes` 还会把禁用节点算进去，不能直接拿来当启动
 * 自动补的目标；否则会误触发，甚至出现“有 pending，但所有目标都被禁用”。
 */
export function startupDepsTargets(report: DepsReport | null): NodeDeps[] {
  if (!report) return []
  return report.nodes.filter(
    (node) => node.enabled && (node.pending.length > 0 || node.manual.length > 0),
  )
}
