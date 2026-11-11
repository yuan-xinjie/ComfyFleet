import { api } from './api'
import logomark from '../assets/logomark.svg'

function base64(bytes: Uint8Array): string {
  let binary = ''
  const chunk = 0x8000
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunk))
  }
  return btoa(binary)
}

/**
 * 托盘使用 logomark.svg 原图，画布保持透明。
 * 不加圆角底、不加描边，也不跟随主题色重绘。
 */
export async function syncTrayIcon(): Promise<void> {
  const size = 64
  const canvas = document.createElement('canvas')
  canvas.width = size
  canvas.height = size
  const ctx = canvas.getContext('2d')
  if (!ctx) return

  const img = new Image()
  img.src = logomark
  await img.decode()
  const markHeight = 50
  const markWidth = (img.naturalWidth / img.naturalHeight) * markHeight
  ctx.drawImage(img, (size - markWidth) / 2, (size - markHeight) / 2, markWidth, markHeight)

  const rgba = ctx.getImageData(0, 0, size, size).data
  await api.setTrayIcon(base64(new Uint8Array(rgba.buffer)))
}
