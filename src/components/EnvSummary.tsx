import type { EnvReport } from '../types'

/** 一个环境的只读摘要：Python / torch / CUDA。 */
export default function EnvSummary({ env }: { env: EnvReport }) {
  return (
    <div className="env-summary">
      <div className="env-summary-line">
        <span className="badge ghost">Python {env.python || '?'}</span>
        <span className="badge ghost">{env.abi}</span>
        <span className="badge ghost">{env.platform}</span>
        <span className="badge ghost">torch {env.torch ?? '未装'}</span>
        <span className="badge ghost">CUDA {env.torchCuda ?? '无'}</span>
      </div>
    </div>
  )
}
