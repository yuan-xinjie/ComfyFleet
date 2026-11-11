import type { InstanceConfig } from '../types'

/** ComfyUI 本体始终在实例根目录的 ComfyUI 子目录里（导入时也统一成这个布局）。 */
export function comfyPath(
  inst: InstanceConfig,
  child: 'models' | 'custom_nodes' | 'output',
): string {
  return `${inst.rootDir.replace(/[\\/]+$/, '')}\\ComfyUI\\${child}`
}
