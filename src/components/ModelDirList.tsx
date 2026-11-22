import { useState } from 'react'
import { PathInput, TextInput } from './Form'
import Button from './Button'
import IconTextButton from './IconTextButton'
import { api } from '../lib/api'
import { MODEL_TYPES, emptyModelDir } from '../lib/storage'
import type { ModelDir } from '../types'
import Icon from './Icon'
import Select from './Select'

/** Select 里的「自定义分类…」哨兵值，不是真实类型 */
const CUSTOM_TYPE = '__custom_category__'

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
 * 一行 = 一个目录；`type` 为空表示 models 根目录（下面按子目录名找类型），
 * 选了具体类型就只认那一个；类型下拉里可以输入自定义分类。
 */
export default function ModelDirList({
  dirs,
  onChange,
  showDefault = false,
  disabled = false,
  onError,
}: Props) {
  const [customEdit, setCustomEdit] = useState<{ index: number; value: string } | null>(null)

  const set = (index: number, patch: Partial<ModelDir>) =>
    onChange(dirs.map((dir, i) => (i === index ? { ...dir, ...patch } : dir)))

  const remove = (index: number) => onChange(dirs.filter((_, i) => i !== index))

  const commitCustom = () => {
    const edit = customEdit
    if (!edit) return
    const type = edit.value.trim()
    setCustomEdit(null)
    if (type && type !== CUSTOM_TYPE) set(edit.index, { type })
  }

  const add = () => onChange([...dirs, emptyModelDir()])

  const makeDefault = (index: number) =>
    onChange(dirs.map((dir, i) => ({ ...dir, isDefault: i === index })))

  const open = (path: string) => {
    const target = path.trim()
    if (!target) return
    api.openPath(target).catch((e) => onError?.(String((e as Error)?.message ?? e)))
  }

  const pick = async (index: number) => {
    try {
      const folder = await api.pickFolder()
      if (folder) set(index, { path: folder })
    } catch (e) {
      onError?.(String((e as Error)?.message ?? e))
    }
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
          <PathInput
            className="mono dir-path"
            aria-label="模型目录路径"
            placeholder="D:\\models 或 D:\\sd\\checkpoints"
            value={dir.path}
            spellCheck={false}
            disabled={disabled}
            pickTitle="选择模型目录"
            pickDisabled={disabled}
            onPick={() => void pick(index)}
            onChange={(e) => set(index, { path: e.target.value })}
          />
          {customEdit?.index === index ? (
            <TextInput
              className="dir-type-input"
              aria-label="自定义模型分类"
              placeholder="分类名"
              value={customEdit.value}
              autoFocus
              spellCheck={false}
              disabled={disabled}
              onFocus={(event) => event.currentTarget.select()}
              onChange={(event) =>
                setCustomEdit({ index, value: event.target.value })
              }
              onBlur={commitCustom}
              onKeyDown={(event) => {
                if (event.key === 'Enter') {
                  event.preventDefault()
                  commitCustom()
                } else if (event.key === 'Escape') {
                  event.preventDefault()
                  setCustomEdit(null)
                }
              }}
            />
          ) : (
            <Select
              className="dir-type"
              ariaLabel="模型目录类型"
              value={dir.type ?? ''}
              disabled={disabled}
              onChange={(value) => {
                if (value === CUSTOM_TYPE) {
                  setCustomEdit({ index, value: dir.type ?? '' })
                  return
                }
                setCustomEdit(null)
                set(index, { type: value || null })
              }}
              options={[
                { value: '', label: 'models 根目录' },
                ...(dir.type &&
                dir.type !== CUSTOM_TYPE &&
                !(MODEL_TYPES as readonly string[]).includes(dir.type)
                  ? [{ value: dir.type, label: dir.type }]
                  : []),
                { value: CUSTOM_TYPE, label: '自定义分类…' },
                ...MODEL_TYPES.map((type) => ({ value: type, label: type })),
              ]}
            />
          )}
          {showDefault && (
            <IconTextButton
              type="button"
              className={`ghost sm dir-default${dir.isDefault ? ' on' : ''}`}
              aria-label={dir.isDefault ? '当前下载落点' : '设为下载落点'}
              title="新模型下载 / 保存默认落到这个目录（写进 yaml 的 is_default）"
              disabled={disabled}
              onClick={() => makeDefault(index)}
             icon={dir.isDefault ? 'check' : 'download'} iconSize={13}>
              
              {dir.isDefault ? '下载落点' : '设为下载落点'}
            </IconTextButton>
          )}
          <Button
            type="button"
            className="ghost sm icon-btn"
            aria-label="打开目录"
            title={dir.path.trim() ? `打开 ${dir.path}` : '先填路径'}
            disabled={disabled || !dir.path.trim()}
            onClick={() => open(dir.path)}
          >
            <Icon name="folder" size={13} />
          </Button>
          <Button
            type="button"
            className="ghost sm icon-btn danger"
            aria-label="移除目录"
            title="从这个列表里去掉（不会删除磁盘上的文件）"
            disabled={disabled}
            onClick={() => remove(index)}
          >
            <Icon name="trash" size={13} />
          </Button>
        </div>
      ))}
      <div className="dir-actions">
        <IconTextButton type="button" className="ghost sm" onClick={add} disabled={disabled} icon="plus" iconSize={13}>
          
          添加目录
        </IconTextButton>
      </div>
    </div>
  )
}
