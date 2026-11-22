import { forwardRef, type InputHTMLAttributes } from 'react'
import Icon from './Icon'
import TextInput from './TextInput'

export interface PathInputProps extends InputHTMLAttributes<HTMLInputElement> {
  /** 右侧内嵌按钮：打开系统选文件夹；不传就不显示按钮。 */
  onPick?: () => void
  pickTitle?: string
  pickDisabled?: boolean
}

/**
 * 路径输入框：文本 + 右侧内嵌「选择文件夹」按钮。
 *
 * 按钮压在输入框内，行里不再单占一个按钮位；输入框右侧留出按钮宽度，
 * 长路径不会被按钮盖住。
 */
const PathInput = forwardRef<HTMLInputElement, PathInputProps>(function PathInput(
  { className = '', onPick, pickTitle = '选择文件夹', pickDisabled = false, ...rest },
  ref,
) {
  return (
    <div className="path-input">
      <TextInput ref={ref} className={className} {...rest} />
      {onPick && (
        <button
          type="button"
          className="path-input-pick"
          title={pickTitle}
          aria-label={pickTitle}
          disabled={pickDisabled}
          onClick={onPick}
        >
          <Icon name="folder" size={13} />
        </button>
      )}
    </div>
  )
})

export default PathInput
