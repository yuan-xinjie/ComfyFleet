import { forwardRef, type InputHTMLAttributes } from 'react'

export interface TextInputProps extends InputHTMLAttributes<HTMLInputElement> {
  /** 等宽字体，用于路径、URL、端口等机器值。 */
  mono?: boolean
}

const TextInput = forwardRef<HTMLInputElement, TextInputProps>(function TextInput(
  { mono = false, className = '', ...rest },
  ref,
) {
  const classes = [mono ? 'mono' : '', className].filter(Boolean).join(' ')
  return <input ref={ref} className={classes || undefined} {...rest} />
})

export default TextInput
