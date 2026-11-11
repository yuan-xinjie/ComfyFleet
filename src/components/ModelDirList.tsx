import { api } from '../lib/api'
import { MODEL_TYPES, emptyModelDir, modelTypeLabel } from '../lib/storage'
import type { ModelDir } from '../types'
import Icon from './Icon'
import Select from './Select'

interface Props {
  dirs: ModelDir[]
  onChange: (dirs: ModelDir[]) => void
  /** 是否显示「下载落点」按钮（只有全局共享目录用得上） */
  showDefault?: boolean
  /** 关掉编辑（比如导入的实例） */
  disabled?: boolean
  onError?: (msg: string) => void
}

/**
 * 模型目录列表编辑器，全局共享与实例附加共用。
 *
 * 一行 = 一个目录；`type` 为空表示整包 models 根目录（下面按子目录名找类型），
 * 选了具体类型就只认那一个。
 */
export default function ModelDirList({
  dirs,
  onChange,
  showDefault = false,
  disabled = false,
  onError,
}: Props) {
  const set = (index: number, patch: Partial<ModelDir>) =>
    onChange(dirs.map((dir, i) => (i === index ? { ...dir, ...patch } : dir)))

  const remove = (index: number) => onChange(dirs.filter((_, i) => i !== index))

  const add = () => onChange([...dirs, emptyModelDir()])

  const makeDefault = (index: number) =>
    onChange(dirs.map((dir, i) => ({ ...dir, isDefault: i === index })))

  const open = (path: string) => {
    const target = path.trim()
    if (!target) return
    api.openPath(target).catch((e) => onError?.(String((e as Error)?.message ?? e)))
  }

  return (
    <div className="dir-list">
      {dirs.length === 0 && (
        <div className="dir-empty faint">
          还没有目录。
        </div>
      )}
      {dirs.map((dir, index) => (
        <div className="dir-row" key={index}>
          <input
            className="mono dir-path"
            aria-label="模型目录路径"
            placeholder="D:\\models 或 D:\\sd\\checkpoints"
            value={dir.path}
            spellCheck={false}
            disabled={disabled}
            onChange={(e) => set(index, { path: e.target.value })}
          />
          <Select
            className="dir-type"
            ariaLabel="模型目录类型"
            value={dir.type ?? ''}
            disabled={disabled}
            onChange={(value) => set(index, { type: value || null })}
            options={[
              { value: '', label: '整包 models 根目录' },
              ...MODEL_TYPES.map((type) => {
                const [label, ...rest] = modelTypeLabel(type).split(' · ')
                return { value: type, label, meta: rest.join(' · ') || undefined }
              }),
            ]}
          />
          {showDefault && (
            <button
              type="button"
              className={`ghost sm dir-default${dir.isDefault ? ' on' : ''}`}
              aria-label={dir.isDefault ? '当前下载落点' : '设为下载落点'}
              title="新模型下载 / 保存默认落到这个目录（写进 yaml 的 is_default）"
              disabled={disabled}
              onClick={() => makeDefault(index)}
            >
              <Icon name={dir.isDefault ? 'check' : 'download'} size={13} />
              {dir.isDefault ? '下载落点' : '设为下载落点'}
            </button>
          )}
          <button
            type="button"
            className="ghost sm icon-btn"
            aria-label="打开目录"
            title={dir.path.trim() ? `打开 ${dir.path}` : '先填路径'}
            disabled={disabled || !dir.path.trim()}
            onClick={() => open(dir.path)}
          >
            <Icon name="folder" size={13} />
          </button>
          <button
            type="button"
            className="ghost sm icon-btn danger"
            aria-label="移除目录"
            title="从这个列表里去掉（不会删除磁盘上的文件）"
            disabled={disabled}
            onClick={() => remove(index)}
          >
            <Icon name="trash" size={13} />
          </button>
        </div>
      ))}
      <div className="dir-actions">
        <button type="button" className="ghost sm" onClick={add} disabled={disabled}>
          <Icon name="plus" size={13} />
          添加目录
        </button>
      </div>
    </div>
  )
}
