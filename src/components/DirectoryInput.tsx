import { forwardRef, type InputHTMLAttributes, type ReactNode } from 'react'
import Button from './Button'
import Icon from './Icon'
import PathInput from './PathInput'

export interface DirectoryInputProps extends InputHTMLAttributes<HTMLInputElement> {
  /** 外层布局类，默认沿用表单的 inline-input。 */
  wrapperClassName?: string
  /** 输入框右侧内嵌的选文件夹按钮；不传则不显示。 */
  onPick?: () => void
  pickTitle?: string
  pickDisabled?: boolean
  /** 打开当前目录；不传则不显示文件夹按钮。 */
  onOpen?: () => void
  openTitle?: string
  openDisabled?: boolean
  /** 选择/打开之后追加的操作按钮，例如「检测」「用默认位置」。 */
  actions?: ReactNode
}

/**
 * 目录/路径输入的统一外壳：内嵌选文件夹按钮 + 额外的打开/操作按钮。
 *
 * 输入框本身走 PathInput，外层沿用现有的 inline-input / dir-single / dir-row
 * 布局类，避免路径行各自拼一套按钮。
 */
const DirectoryInput = forwardRef<HTMLInputElement, DirectoryInputProps>(
  function DirectoryInput(
    {
      className = '',
      wrapperClassName = 'inline-input',
      onPick,
      pickTitle,
      pickDisabled = false,
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
        <PathInput
          ref={ref}
          className={className}
          onPick={onPick}
          pickTitle={pickTitle}
          pickDisabled={pickDisabled}
          {...rest}
        />
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
