import { forwardRef, type InputHTMLAttributes, type ReactNode } from 'react'
import Button from './Button'
import Icon from './Icon'
import TextInput from './TextInput'

export interface DirectoryInputProps extends InputHTMLAttributes<HTMLInputElement> {
  /** 外层布局类，默认沿用表单的 inline-input。 */
  wrapperClassName?: string
  /** 打开系统目录选择器；不传则不显示选择按钮。 */
  onPick?: () => void
  pickLabel?: string
  pickTitle?: string
  pickDisabled?: boolean
  pickIcon?: boolean
  /** 打开当前目录；不传则不显示文件夹按钮。 */
  onOpen?: () => void
  openTitle?: string
  openDisabled?: boolean
  /** 选择/打开之后追加的操作按钮，例如「检测」「用默认位置」。 */
  actions?: ReactNode
}

/**
 * 目录/路径输入的统一外壳：文本框 + 选择目录 + 打开目录 + 额外操作。
 *
 * 输入框本身仍走 TextInput，外层沿用现有的 inline-input / dir-single / dir-row
 * 布局类，避免路径行各自拼一套按钮。
 */
const DirectoryInput = forwardRef<HTMLInputElement, DirectoryInputProps>(
  function DirectoryInput(
    {
      className = '',
      wrapperClassName = 'inline-input',
      onPick,
      pickLabel = '选择',
      pickTitle,
      pickDisabled = false,
      pickIcon = true,
      onOpen,
      openTitle = '打开目录',
      openDisabled = false,
      actions,
      ...rest
    },
    ref,
  ) {
    return (
      <div className={wrapperClassName}>
        <TextInput ref={ref} className={className} {...rest} />
        {onPick && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            onClick={onPick}
            disabled={pickDisabled}
            title={pickTitle}
            aria-label={pickTitle ?? pickLabel}
          >
            {pickIcon && <Icon name="folder" size={13} />}
            {pickLabel}
          </Button>
        )}
        {actions}
        {onOpen && (
          <Button
            type="button"
            variant="ghost"
            size="sm"
            icon
            onClick={onOpen}
            disabled={openDisabled}
            title={openTitle}
            aria-label={openTitle}
          >
            <Icon name="folder" size={13} />
          </Button>
        )}
      </div>
    )
  },
)

export default DirectoryInput
