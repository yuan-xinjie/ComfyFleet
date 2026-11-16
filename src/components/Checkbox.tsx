import { forwardRef, type InputHTMLAttributes } from 'react'

export interface CheckboxProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> {
  type?: 'checkbox'
}

const Checkbox = forwardRef<HTMLInputElement, CheckboxProps>(function Checkbox(
  { className = '', ...rest },
  ref,
) {
  return <input ref={ref} className={className || undefined} {...rest} type="checkbox" />
})

export default Checkbox
