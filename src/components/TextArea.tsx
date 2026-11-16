import { forwardRef, type TextareaHTMLAttributes } from 'react'

export interface TextAreaProps extends TextareaHTMLAttributes<HTMLTextAreaElement> {
  /** 等宽字体，用于环境变量、启动参数等原始文本。 */
  mono?: boolean
}

const TextArea = forwardRef<HTMLTextAreaElement, TextAreaProps>(function TextArea(
  { mono = false, className = '', ...rest },
  ref,
) {
  const classes = [mono ? 'mono' : '', className].filter(Boolean).join(' ')
  return <textarea ref={ref} className={classes || undefined} {...rest} />
})

export default TextArea
