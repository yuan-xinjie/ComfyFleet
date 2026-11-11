export type RunProgress = {
  what: string
  value: number
  max: number
  speed: string | null
}

export type LogTaskState = {
  task: RunProgress | null
  ended: boolean
  stage: string
}

export type TqdmInfo = {
  value: number
  max: number
  speed: string | null
  /** 进度条前面的说明（旧格式 tqdm） */
  desc: string
  /** 时间/速度后面的文字（ComfyUI 把 “Model Initializing ...” 放在这里） */
  postfix: string
}

export const TQDM_RE = /(\d{1,3})%\|[^|]*\|\s*(\d+)\/(\d+)\s*\[([^\]]+)\]/
const TQDM_SCAN_RE = new RegExp(TQDM_RE.source, 'g')

/** 从 ComfyUI / tqdm 的进度条里拿步数和速度。 */
export function parseTqdm(line: string): TqdmInfo | null {
  // 老日志里可能残留一整串用 \r 串起来的更新，取最后一条才是当前进度
  let m: RegExpExecArray | null = null
  for (const match of line.matchAll(TQDM_SCAN_RE)) m = match
  if (!m) return null
  const value = Number(m[2])
  const max = Number(m[3])
  if (!Number.isFinite(value) || !Number.isFinite(max) || value < 0 || max <= 0) return null
  let speed: string | null = null
  const sm = m[4].match(/([\d.]+)\s*(it\/s|s\/it)/i)
  if (sm) {
    const n = Number(sm[1])
    if (Number.isFinite(n) && n > 0) {
      speed = sm[2].toLowerCase() === 's/it' ? `${(1 / n).toFixed(2)} it/s` : `${n.toFixed(2)} it/s`
    }
  }
  const prefix = line.slice(0, m.index).split('\r').pop() ?? ''
  const desc = prefix.trim().replace(/^\[[^\]]{1,40}\]\s*/, '').trim()
  const postfix = m[4].trim()
  return { value, max, speed, desc, postfix }
}

/** tqdm 的说明可能在前面，也可能像 ComfyUI 一样跟在速度后面。 */
export function tqdmStageText(p: TqdmInfo): string {
  return [p.desc, p.postfix].filter(Boolean).join(' ')
}

export function stageFromText(text: string, fallback: string): string {
  const t = text.toLowerCase()
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
  if (/loaded completely|model loaded/.test(lower)) return '采样'
  if (/vae.*decode|decoding/.test(lower)) return '解码'
  if (/vae.*encode|encoding/.test(lower)) return '编码'
  // 采样结束后的 “Requested to load AutoencodingEngine” 是在准备 VAE 解码，
  // 不应再显示成加载模型。
  if (/autoencodingengine/.test(lower)) return '解码'
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

export function progressFromTqdm(p: TqdmInfo, stage: string): RunProgress {
  return {
    what: stage === '采样' ? `正在采样 ${Math.round(p.value)}/${Math.round(p.max)}` : `正在${stage}`,
    value: p.value,
    max: p.max,
    speed: p.speed,
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

  for (const raw of lines) {
    const line = raw.trim()
    if (!line) continue
    const p = parseTqdm(raw)
    if (p) {
      stage = stageFromText(tqdmStageText(p), stage)
      ended = false
      task = progressFromTqdm(p, stage)
      continue
    }
    if (isTaskEnd(line)) {
      task = null
      ended = true
      stage = '采样'
      continue
    }
    if (isTaskStart(line)) {
      ended = false
      stage = '准备'
      continue
    }
    stage = stageFromLine(line, stage)
  }

  return { task: ended ? null : task, ended, stage }
}
