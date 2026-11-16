import { forwardRef, type InputHTMLAttributes } from 'react'

export interface RangeProps extends Omit<InputHTMLAttributes<HTMLInputElement>, 'type'> {
  type?: 'range'
}

const Range = forwardRef<HTMLInputElement, RangeProps>(function Range(
  { className = '', ...rest },
  ref,
) {
  return <input ref={ref} className={className || undefined} {...rest} type="range" />
})

export default Range
