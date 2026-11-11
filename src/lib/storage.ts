import type { ModelDir } from '../types'

/**
 * ComfyUI 的模型类型。这里列的是官方自带的类型，
 * 生成 yaml 时真正的依据是磁盘上扫到的子目录名——多出来的会被 ComfyUI 静默忽略，
 * 少了的（新版本新类型）也能照常工作，这个列表只用来做「类型」下拉。
 */
export const MODEL_TYPES = [
  'checkpoints',
  'diffusion_models',
  'text_encoders',
  'clip',
  'clip_vision',
  'vae',
  'vae_approx',
  'loras',
  'controlnet',
  'upscale_models',
  'embeddings',
  'style_models',
  'gligen',
  'hypernetworks',
  'photomaker',
  'configs',
] as const

/** 常见类型的白话名字：下拉里带一句，免得对着英文猜 */
export const MODEL_TYPE_LABEL: Record<string, string> = {
  checkpoints: '主模型',
  diffusion_models: '扩散模型 / UNet',
  text_encoders: '文本编码器（CLIP-L/T5）',
  clip: 'CLIP（老名字）',
  clip_vision: 'CLIP 视觉',
  vae: 'VAE',
  vae_approx: 'VAE 近似预览',
  loras: 'LoRA',
  controlnet: 'ControlNet',
  upscale_models: '放大模型',
  embeddings: 'Embedding / 文本反转',
  style_models: '风格模型',
  gligen: 'GLIGEN',
  hypernetworks: 'Hypernetwork',
  photomaker: 'PhotoMaker',
  configs: '配置文件',
}

export function modelTypeLabel(type: string): string {
  const label = MODEL_TYPE_LABEL[type]
  return label ? `${type} · ${label}` : type
}

export function emptyModelDir(isDefault = false): ModelDir {
  return { path: '', type: null, isDefault }
}

/** 目录归属的中文说法，两个页面共用 */
export const SOURCE_LABEL: Record<string, string> = {
  own: '实例自己的',
  extra: '实例附加',
  shared: '全局共享',
}

export const DIR_MODE_LABEL: Record<string, string> = {
  own: '用实例自己的',
  shared: '用共享目录',
  custom: '自定义目录',
}
