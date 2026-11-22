import type { PythonProgress } from '../types'

export type RunProgress = {
  /** 进度来源 id；多进度条列表用它做稳定 key。 */
  id?: string
  what: string
  /** 当前阶段的固定说法，更新步数/百分比时不变。 */
  label?: string
  /** 会随进度变化的短信息（步数、百分比）。 */
  detail?: string
  value: number
  max: number
  speed: string | null
}

export type LogTaskState = {
  task: RunProgress | null
  ended: boolean
  stage: string
  /** 日志里是否已经出现本轮任务的开始/进度行。 */
  started: boolean
}

export type TqdmInfo = {
  value: number
  max: number
  speed: string | null
  /** 进度条前面的说明（旧格式 tqdm） */
  desc: string
  /** 时间/速度后面的文字（ComfyUI 把 “Model Initializing ...” 放在这里） */
  postfix: string
  /** 是否按字节计数（下载进度），而不是步数/文件数 */
  bytes: boolean
}

/** tqdm 的计数可能是 5/20，也可能是下载的 2.3G/5.1G。 */
const TQDM_NUMBER = String.raw`\d+(?:\.\d+)?`
const TQDM_UNIT = String.raw`(?:[KMGTPE]i?B?|B)`
export const TQDM_RE = new RegExp(
  String.raw`(\d{1,3})%\|[^|]*\|\s*(${TQDM_NUMBER})\s*(${TQDM_UNIT})?\s*\/\s*(${TQDM_NUMBER})\s*(${TQDM_UNIT})?\s*\[([^\]]+)\]`,
  'i',
)
/** 整条 tqdm 更新（含前面的 desc 和后面的 postfix），控制台用。 */
export const TQDM_LINE_RE = new RegExp(
  String.raw`\r?[^\r\n]*${TQDM_RE.source}[^\r\n]*\r?\n?`,
  'gi',
)
const TQDM_SCAN_RE = new RegExp(TQDM_RE.source, 'gi')

const DOWNLOAD_WORD_RE = /\bdownload(?:ing)?\b|下载|拉取/i
const FETCH_WORD_RE = /\bfetch(?:ing)?\b/i
const MODEL_WORD_RE =
  /\b(?:models?|checkpoints?|weights?|loras?|vae|clip|unet|diffusion|safetensors|gguf|onnx|pth|ckpt|bin)\b|模型/i
const DOWNLOAD_END_RE =
  /\bdownloaded\b|download(?:ed|ing)?\s+(?:complete|completed|finished)|finished downloading|download succeeded|下载完成|已下载完成/i

/** fetch 只有在明确带模型名时才算下载；ComfyUI-Manager 的 FETCH 元数据不是。 */
function looksLikeDownload(text: string): boolean {
  if (/\[\s*done\s*\]/i.test(text)) return false
  return DOWNLOAD_WORD_RE.test(text) || (FETCH_WORD_RE.test(text) && MODEL_WORD_RE.test(text))
}

function sizeUnitMultiplier(unit: string | undefined): number {
  if (!unit) return 1
  const key = unit.toUpperCase()
  const power = 'KMGTPE'.indexOf(key[0])
  return power >= 0 ? 1024 ** (power + 1) : 1
}

function isByteUnit(unit: string | undefined): boolean {
  return Boolean(unit) && /^[KMGTPE]i?B?$|^B$/i.test(unit!.trim())
}

/** 从 ComfyUI / tqdm 的进度条里拿步数、字节数、速度和说明。 */
export function parseTqdm(line: string): TqdmInfo | null {
  // 老日志里可能残留一整串用 \r 串起来的更新，取最后一条才是当前进度
  let m: RegExpExecArray | null = null
  for (const match of line.matchAll(TQDM_SCAN_RE)) m = match
  if (!m) return null
  const valueUnit = m[3]
  const maxUnit = m[5]
  const bytes = isByteUnit(valueUnit) || isByteUnit(maxUnit)
  const value = Number(m[2]) * sizeUnitMultiplier(valueUnit)
  const max = Number(m[4]) * sizeUnitMultiplier(maxUnit)
  if (!Number.isFinite(value) || !Number.isFinite(max) || value < 0 || max <= 0) return null
  let speed: string | null = null
  // 普通进度是 it/s、s/it；下载进度是 MB/s、MiB/s。
  const sm = m[6].match(/([\d.]+)\s*(it\/s|s\/it|[KMGTPE]i?B\/s|[KMGT]?B\/s)/i)
  if (sm) {
    const n = Number(sm[1])
    if (Number.isFinite(n) && n > 0) {
      if (/it\/s|s\/it/i.test(sm[2])) {
        // 和原来的采样速度显示保持一致：慢的时候 tqdm 用 s/it，界面统一成 it/s。
        speed = sm[2].toLowerCase() === 's/it' ? `${(1 / n).toFixed(2)} it/s` : `${n.toFixed(2)} it/s`
      } else {
        // 下载速度保留日志里的单位和精度，避免把 MB/s 换算成别的单位。
        speed = `${sm[1]} ${sm[2]}`
      }
    }
  }
  const prefix = line.slice(0, m.index).split('\r').pop() ?? ''
  const desc = prefix.trim().replace(/^\[[^\]]{1,40}\]\s*/, '').trim()
  // ComfyUI 的模型初始化状态可能写在括号里的速度后面，也可能落在右括号之后。
  const postfix = [m[6].trim(), line.slice(m.index + m[0].length).trim()]
    .filter(Boolean)
    .join(' ')
  return { value, max, speed, desc, postfix, bytes }
}

/** tqdm 的说明可能在前面，也可能像 ComfyUI 一样跟在速度后面。 */
export function tqdmStageText(p: TqdmInfo): string {
  return [p.desc, p.postfix].filter(Boolean).join(' ')
}

export function isDownloadStage(stage: string): boolean {
  return stage === '下载模型' || stage === '下载'
}

function downloadStageFromText(text: string): string | null {
  if (!looksLikeDownload(text)) return null
  return MODEL_WORD_RE.test(text) ? '下载模型' : '下载'
}

/** 从 tqdm 的说明里判断它是不是下载；字节计数是下载最强的信号。 */
export function isDownloadTqdm(info: TqdmInfo): boolean {
  if (info.bytes) return true
  return looksLikeDownload(tqdmStageText(info))
}

export function stageFromTqdm(info: TqdmInfo): string {
  const text = tqdmStageText(info)
  const downloadStage = downloadStageFromText(text)
  if (downloadStage) return downloadStage
  // 按字节推进的进度条基本就是文件下载；ComfyUI 的缺模型下载就用这种条。
  if (info.bytes) return '下载模型'
  return ''
}

/** 把内部阶段名转成任务条上的说法；没有阶段说明的原生节点带出类名。 */
export function stageLabel(stage: string, node?: string): string {
  if (stage === '下载模型') return '正在下载模型'
  if (stage === '下载') return '正在下载文件'
  if (stage === '加载模型') return '正在初始化模型'
  if (stage === '加载视频') return '正在加载视频'
  if (stage === '加载Lora') return '正在加载 Lora'
  if (stage === '加载扩散模型') return '正在加载扩散模型'
  if (stage === '加载VAE') return '正在加载 VAE'
  if (stage === '加载TextEncoder') return '正在加载 TextEncoder'
  if (stage === '加载CLIP') return '正在加载 CLIP'
  if (stage === 'CLIP文本编码') return '正在编码 CLIP 文本'
  if (stage === '保存图像') return '正在保存图像'
  if (stage === '预览图像') return '正在预览图像'
  if (stage === '创建视频') return '正在创建视频'
  if (stage === '保存视频') return '正在保存视频'
  if (stage === '采样') return '正在采样'
  if (stage === '编码') return '正在 VAE 编码'
  if (stage === '解码') return '正在 VAE 解码'
  if (stage === '准备') return '正在准备'
  if (stage === '执行') return node ? `正在执行 ${node}` : '正在执行'
  return node ? `正在执行 ${node}` : '正在执行'
}

export function stageFromText(text: string, fallback: string): string {
  const downloadStage = downloadStageFromText(text)
  if (downloadStage) return downloadStage
  const t = text.toLowerCase()
  // “AutoencodingEngine” 里含有 encoding，必须先按 VAE 解码识别。
  if (/autoencodingengine/.test(t)) return '解码'
  if (/decode|decoding/.test(t)) return '解码'
  if (/encode|encoding/.test(t)) return '编码'
  // ComfyUI 把模型初始化提示写在 tqdm 的 postfix 里：
  // 0% 时是 “Model Initializing ...”，12% 时变成 “Model Initialization complete!”，
  // 后者已经进入采样，不能再按 "model" 判成加载模型。
  if (/initializ\w*\s+complete|model ready|prepared to sample/.test(t)) return '采样'
  if (/sampl|ksampl|step|noise/.test(t)) return '采样'
  if (/load|checkpoint|unet|clip|diffusion|model/.test(t)) return '加载模型'
  return fallback
}

/** 从一行普通日志里推断当前阶段；推断不出来就保留上一阶段。 */
export function stageFromLine(line: string, fallback: string): string {
  const lower = line.toLowerCase()
  if (DOWNLOAD_END_RE.test(line)) return '加载模型'
  const downloadStage = downloadStageFromText(line)
  if (downloadStage) return downloadStage
  if (/loaded completely|model loaded/.test(lower)) return '采样'
  // 采样结束后的 “Requested to load AutoencodingEngine” 是在准备 VAE 解码；
  // 它自身含有 encoding，必须先于 encode 正则识别。
  if (/autoencodingengine/.test(lower)) return '解码'
  if (/vae.*decode|decoding/.test(lower)) return '解码'
  if (/vae.*encode|encoding/.test(lower)) return '编码'
  if (/ksampl|sampling|noise|steps?/.test(lower)) return '采样'
  if (/requested to load|loading model|checkpoint|unet|clip|diffusion|model.*load/.test(lower)) {
    return '加载模型'
  }
  return fallback
}

export function isTaskEnd(line: string): boolean {
  return /prompt executed|execution (success|error|complete|finished|interrupted)|prompt done|exception during processing/i.test(
    line,
  )
}

export function isTaskStart(line: string): boolean {
  return /got prompt|prompt received|execution start(ed)?/i.test(line)
}

function percentOf(value: number, max: number): number | null {
  if (!Number.isFinite(value) || !Number.isFinite(max) || max <= 0) return null
  return Math.round(Math.max(0, Math.min(100, (value / max) * 100)))
}

/** 没有 tqdm 条、但日志里带下载百分比/速度时的兜底。 */
export function parseDownloadLine(line: string): { stage: string; task: RunProgress } | null {
  const text = line.trim()
  if (!text || !looksLikeDownload(text) || DOWNLOAD_END_RE.test(text)) return null
  if (
    /download(?:ing)?\s+(?:failed|error)|failed to download|unable to download|下载失败/i.test(
      text,
    )
  ) {
    return null
  }
  const stage = downloadStageFromText(text) ?? '下载'
  const percentMatch = text.match(/(\d{1,3}(?:\.\d+)?)\s*%/)
  const sizeMatch = text.match(
    new RegExp(
      String.raw`(${TQDM_NUMBER})\s*(${TQDM_UNIT})\s*\/\s*(${TQDM_NUMBER})\s*(${TQDM_UNIT})`,
      'i',
    ),
  )
  const speedMatch = text.match(/([\d.]+)\s*([KMGTPE]i?B|[KMGT]?B)\s*\/s/i)
  let value = 0
  let max = 0
  if (percentMatch) {
    value = Number(percentMatch[1])
    max = 100
  } else if (sizeMatch) {
    value = Number(sizeMatch[1]) * sizeUnitMultiplier(sizeMatch[2])
    max = Number(sizeMatch[3]) * sizeUnitMultiplier(sizeMatch[4])
  }
  if (!Number.isFinite(value) || !Number.isFinite(max) || max <= 0) {
    value = 0
    max = 0
  }
  const speed = speedMatch ? `${speedMatch[1]} ${speedMatch[2]}/s` : null
  const percent = percentOf(value, max)
  const label = stageLabel(stage)
  return {
    stage,
    task: {
      what: percent === null ? label : `${label} ${percent}%`,
      label,
      detail: percent === null ? undefined : `${percent}%`,
      value,
      max,
      speed,
    },
  }
}

export function progressFromTqdm(p: TqdmInfo, stage: string): RunProgress {
  const label = stageLabel(stage)
  if (isDownloadStage(stage)) {
    const percent = percentOf(p.value, p.max)
    return {
      what: percent === null ? label : `${label} ${percent}%`,
      label,
      detail: percent === null ? undefined : `${percent}%`,
      value: p.value,
      max: p.max,
      speed: p.speed,
    }
  }
  const hasSteps = stage === '采样' || stage === '编码' || stage === '解码'
  const step = hasSteps ? `${Math.round(p.value)}/${Math.round(p.max)}` : ''
  return {
    what: step ? `${label} ${step}` : label,
    label,
    detail: step || undefined,
    value: p.value,
    max: p.max,
    speed: p.speed,
  }
}

/** 从左/右文本里取速度；Python 桥通常已经给出 speed，这里是兜底。 */
export function speedFromRight(text: string): string | null {
  const m = text.match(/([\d.]+)\s*(it\/s|s\/it|[KMGTPE]i?B\/s|[KMGT]?B\/s)/i)
  if (!m) return null
  const n = Number(m[1])
  if (!Number.isFinite(n) || n <= 0) return null
  if (/it\/s|s\/it/i.test(m[2])) {
    return m[2].toLowerCase() === 's/it' ? `${(1 / n).toFixed(2)} it/s` : `${n.toFixed(2)} it/s`
  }
  return `${m[1]} ${m[2]}`
}

function nativeStage(name: string): string {
  const lower = name.toLowerCase()
  // 视频、Lora 和常见加载器按明确的类名优先匹配；没列出的原生节点
  // 统一落到“正在执行 <节点名>”。
  if (lower.startsWith('vhs_loadvideo') || lower.startsWith('loadvideo')) return '加载视频'
  if (lower.includes('lora')) return '加载Lora'
  if (lower.startsWith('unetloader')) return '加载扩散模型'
  if (lower.startsWith('vaeloader')) return '加载VAE'
  if (lower.startsWith('dualcliploader')) return '加载CLIP'
  if (lower.startsWith('cliploader')) return '加载TextEncoder'
  if (lower.includes('textencode')) return 'CLIP文本编码'
  if (lower.startsWith('saveimage')) return '保存图像'
  if (lower.startsWith('previewimage')) return '预览图像'
  if (lower.startsWith('createvideo')) return '创建视频'
  if (lower.startsWith('savevideo') || lower.includes('videocombine')) return '保存视频'

  const sampler =
    !lower.includes('select') &&
    (lower.includes('ksampler') ||
      (lower.startsWith('sampler') &&
        (lower.includes('custom') || lower.includes('advanced'))))
  if (sampler) return '采样'
  if (/autoencodingengine|vae.*decode|vaedecode/.test(lower)) return '解码'
  if (/vae.*encode|vaeencode/.test(lower)) return '编码'
  return '执行'
}

/** 从 Python 桥的 name/left/right 推断阶段；推断不出就用 fallback。 */
export function structuredStage(p: PythonProgress, fallback = '准备'): string {
  // 原生节点只按类名判断，禁止拿 left/right 文本参与，避免步数/百分比里
  // 偶然出现的 encode/decode 把节点阶段带偏。
  if (p.id.startsWith('comfy:')) return nativeStage(p.name)
  const text = [p.name, p.left, p.right].filter(Boolean).join(' ')
  const downloadStage = downloadStageFromText(text)
  if (downloadStage) return downloadStage
  // 没有描述、但速度是字节单位，基本就是文件下载。
  if (/(?:[KMGTPE]i?B|B)\/s/i.test(p.speed ?? p.right)) return '下载'
  return stageFromText(text, fallback)
}

/** ComfyUI 原生进度条比 tqdm 更准确；两边同时存在时不要让它们互相抢显示。 */
export function latestProgress(progress: PythonProgress[]): PythonProgress | null {
  if (progress.length === 0) return null
  const native = progress.filter((p) => p.id.startsWith('comfy:'))
  const pool = native.length > 0 ? native : progress
  return pool[pool.length - 1] ?? null
}

const NODE_STAGE_ORDER = [
  '下载模型',
  '下载',
  '加载视频',
  '加载Lora',
  '加载扩散模型',
  '加载VAE',
  '加载TextEncoder',
  '加载CLIP',
  'CLIP文本编码',
  '加载模型',
  '准备',
  '编码',
  '采样',
  '解码',
  '保存图像',
  '预览图像',
  '创建视频',
  '保存视频',
  '执行',
]

/**
 * 控制台底部用：把当前所有进度排成稳定的多行。
 *
 * 原生节点进度在时，隐藏同阶段的 tqdm 条，避免“正在准备”和“正在采样”
 * 同时出现；模型加载、下载、rich 等非节点进度仍然保留。
 */
export function structuredProgressList(progress: PythonProgress[]): RunProgress[] {
  const hasNative = progress.some((p) => p.id.startsWith('comfy:'))
  const filtered = progress.filter((p) => {
    if (!hasNative || p.id.startsWith('comfy:')) return true
    return isDownloadStage(structuredStage(p))
  })
  // ComfyUI 的节点是串行执行的，同一时刻只可能有一个节点进度。
  // 这里只取最新的原生节点条，防止 finish 事件稍晚到达时短暂出现两条。
  const latestNative = [...filtered]
    .reverse()
    .find((p) => p.id.startsWith('comfy:'))
  const visible = latestNative
    ? [latestNative, ...filtered.filter((p) => !p.id.startsWith('comfy:'))]
    : filtered
  const order = new Map(NODE_STAGE_ORDER.map((stage, index) => [stage, index]))
  return visible
    .map((p) => {
      const item = progressFromStructured(p)
      return { ...item.task, id: p.id, stage: item.stage }
    })
    .sort((a, b) => {
      const sa = order.get(a.stage) ?? NODE_STAGE_ORDER.length
      const sb = order.get(b.stage) ?? NODE_STAGE_ORDER.length
      if (sa !== sb) return sa - sb
      return (a.id ?? '').localeCompare(b.id ?? '')
    })
    .map(({ stage: _stage, ...task }) => task)
}

/** Python 桥的进度和日志解析共用同一个 RunProgress 形状。 */
export function progressFromStructured(
  p: PythonProgress,
  stage = structuredStage(p),
): { stage: string; task: RunProgress } {
  const label = stageLabel(
    stage,
    p.id.startsWith('comfy:') ? p.name : undefined,
  )
  const percent = percentOf(p.value, p.max)
  const hasSteps = stage === '采样' || stage === '编码' || stage === '解码'
  const step =
    hasSteps && p.max > 0
      ? `${Math.round(Math.max(0, p.value))}/${Math.round(p.max)}`
      : ''
  const detail = isDownloadStage(stage)
    ? percent === null
      ? undefined
      : `${percent}%`
    : step || undefined
  const what = detail ? `${label} ${detail}` : label
  return {
    stage,
    task: {
      id: p.id,
      what,
      label,
      detail,
      value: p.value,
      max: p.max,
      speed: p.speed ?? speedFromRight(p.right),
    },
  }
}

/**
 * 只看进程日志里的 tqdm 进度：ComfyUI 的 /ws 会因为 Origin 是 tauri.localhost 被 403，
 * 但 python main.py 的 stdout 里本来就带着完整进度，直接拿它更稳。
 */
export function deriveLogTask(lines: string[]): LogTaskState {
  let stage = '采样'
  let task: RunProgress | null = null
  let ended = false
  let started = false

  for (const raw of lines) {
    const line = raw.trim()
    if (!line) continue
    const p = parseTqdm(raw)
    if (p) {
      stage = stageFromTqdm(p) || stageFromText(tqdmStageText(p), stage)
      ended = false
      started = true
      task = progressFromTqdm(p, stage)
      continue
    }
    const download = parseDownloadLine(line)
    if (download) {
      // 已经知道在下载模型时，后续只带百分比/速度的行不要把它降级成泛泛的“下载”。
      stage = stage === '下载模型' && download.stage === '下载' ? stage : download.stage
      ended = false
      started = true
      task = download.task
      continue
    }
    if (isTaskEnd(line)) {
      task = null
      ended = true
      started = false
      stage = '采样'
      continue
    }
    if (isTaskStart(line)) {
      ended = false
      started = true
      stage = '准备'
      // 新一轮任务开始时丢掉上一轮残留的进度条对象，避免阶段已经
      // 变成 VAE 解码了还挂着旧的“VAE 编码”。
      task = null
      continue
    }
    const nextStage = stageFromLine(line, stage)
    // 普通日志把阶段推进后，旧的 tqdm 任务对象就不再代表当前阶段；
    // 结构化进度会接管，日志兜底只保留新的阶段名。
    if (nextStage !== stage) task = null
    stage = nextStage
  }

  return { task: ended ? null : task, ended, stage, started }
}
