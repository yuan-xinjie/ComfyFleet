import type { EnvReport, RepoWheel } from '../types'

/**
 * 「选择轮子」弹窗里的版本筛选。
 *
 * 只按 wheel 文件名里的标签筛选，不改动安装时的兼容性判断：
 * - CUDA：`cu130` / `cu12` → 130 / 12，大版本标签按前缀匹配；
 * - torch：`torch2.10.0` → 2.10，按 major.minor 比较；
 * - Python：按 pip 的 sys_tags 语义，`abi3` 轮子向下兼容，`py39-none` 轮子
 *   从 3.9 起都能装，`py3` 通用轮则始终显示。
 * 没有对应标签的轮子视为「不受该维度约束」，任何筛选下都保留。
 */
export interface WheelFilters {
  cuda: string
  torch: string
  python: string
}

export const EMPTY_FILTERS: WheelFilters = { cuda: '', torch: '', python: '' }

export interface WheelFilterOption {
  value: string
  label: string
  /** 当前实例用的是这个值时显示「当前」 */
  meta?: string
}

export interface WheelFilterOptions {
  cuda: WheelFilterOption[]
  torch: WheelFilterOption[]
  python: WheelFilterOption[]
}

/** 130 / 12.4 → 130 / 124 */
export function cudaDigits(tag: string | null | undefined): string {
  return (tag ?? '').replace(/[^0-9]/g, '')
}

/** 130 → 13.0，128 → 12.8，12 → 12 */
export function cudaText(digits: string): string {
  if (digits.length <= 2) return digits
  return `${digits.slice(0, 2)}.${digits.slice(2)}`
}

/** 2.10.0 → 2.10；2.10+cu130 → 2.10 */
export function torchMinor(tag: string | null | undefined): string {
  const base = (tag ?? '').split('+')[0]
  const [major, minor] = base.split('.')
  return minor ? `${major}.${minor}` : base
}

/** cp313 / py39 → 3.13 / 3.9；py3 这种通用标签返回 null */
export function pythonVersionOf(tag: string): string | null {
  const match = /^(?:cp|py)(\d)(\d+)$/.exec((tag ?? '').toLowerCase())
  return match ? `${match[1]}.${match[2]}` : null
}

/** 某个 Python 版本能不能装这个轮子（与 accel.rs 的 python_ok 同一套语义） */
function pythonAllows(wheel: RepoWheel, selected: string): boolean {
  const py = wheel.pythonTag
  if (!py) return true
  if (py === 'py3' || py === 'py2.py3') return true

  const [selMajor, selMinor] = selected.split('.').map(Number)
  if (!Number.isFinite(selMajor) || !Number.isFinite(selMinor)) return true

  const cp = /^cp(\d)(\d+)$/.exec(py)
  if (cp) {
    const major = Number(cp[1])
    const minor = Number(cp[2])
    if (wheel.abiTag === 'abi3') return major === selMajor && minor <= selMinor
    return major === selMajor && minor === selMinor
  }

  // py39-none-win_amd64 这类「无 ABI + 平台」的轮子：3.9 及以上都算匹配
  if (wheel.abiTag === 'none') {
    const pyOnly = /^py(\d)(\d+)$/.exec(py)
    if (pyOnly) return Number(pyOnly[1]) === selMajor && Number(pyOnly[2]) <= selMinor
  }
  return false
}

/** 与 accel.rs 的 cuda_ok 一致：大版本标签按前缀匹配 */
function cudaAllows(tag: string | null, selected: string): boolean {
  if (!selected || !tag) return true
  const wheel = cudaDigits(tag)
  if (wheel.length < 2 || selected.length < 2) return true
  if (wheel.length === 2) return selected.startsWith(wheel)
  return wheel === selected
}

function torchAllows(tag: string | null, selected: string): boolean {
  if (!selected || !tag) return true
  return torchMinor(tag) === selected
}

export function wheelMatches(wheel: RepoWheel, filters: WheelFilters): boolean {
  return (
    cudaAllows(wheel.cudaTag, filters.cuda) &&
    torchAllows(wheel.torchTag, filters.torch) &&
    pythonAllows(wheel, filters.python)
  )
}

function compareVersion(a: string, b: string): number {
  const left = a.split('.').map(Number)
  const right = b.split('.').map(Number)
  for (let index = 0; index < Math.max(left.length, right.length); index += 1) {
    const diff = (left[index] ?? 0) - (right[index] ?? 0)
    if (diff !== 0) return diff
  }
  return 0
}

/** 从这组轮子里收集可筛的标签值；当前实例用到的值也放进去并标记「当前」 */
export function filterOptions(wheels: RepoWheel[], env: EnvReport | null): WheelFilterOptions {
  const envCuda = cudaDigits(env?.torchCuda)
  const envTorch = torchMinor(env?.torch)
  const envPython = env?.python ? env.python.split('.').slice(0, 2).join('.') : ''

  // 只有该维度在仓库里真的出现过标签，才把当前环境的值也列出来
  const collect = (values: string[], envValue: string): string[] => {
    const all = [...new Set(values.filter(Boolean))]
    if (all.length > 0 && envValue && !all.includes(envValue)) all.push(envValue)
    return all.sort((a, b) => compareVersion(b, a))
  }

  const cuda = collect(
    wheels.map((wheel) => cudaDigits(wheel.cudaTag)),
    envCuda,
  )
  const torch = collect(
    wheels.map((wheel) => torchMinor(wheel.torchTag)),
    envTorch,
  )
  const python = collect(
    wheels.map((wheel) => pythonVersionOf(wheel.pythonTag) ?? ''),
    envPython,
  )

  return {
    cuda: cuda.map((value) => ({
      value,
      label: `CUDA ${cudaText(value)}`,
      meta: value === envCuda ? '当前' : undefined,
    })),
    torch: torch.map((value) => ({
      value,
      label: `torch ${value}`,
      meta: value === envTorch ? '当前' : undefined,
    })),
    python: python.map((value) => ({
      value,
      label: `Python ${value}`,
      meta: value === envPython ? '当前' : undefined,
    })),
  }
}
