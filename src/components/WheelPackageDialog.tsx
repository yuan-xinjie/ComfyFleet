import { useEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { EnvReport, RepoWheel } from '../types'
import Icon from './Icon'
import Select from './Select'
import {
  EMPTY_FILTERS,
  cudaDigits,
  filterOptions,
  pythonVersionOf,
  torchMinor,
  wheelMatches,
  type WheelFilters,
} from '../lib/wheelFilters'

export interface WheelGroup {
  name: string
  wheels: RepoWheel[]
  compatible: RepoWheel[]
  incompatible: RepoWheel[]
}

interface Props {
  group: WheelGroup
  env: EnvReport | null
  installBlocked: string | null
  downloading: string | null
  downloadError: string | null
  onClose: () => void
  onDownload: (wheel: RepoWheel) => void
  onInstall: (wheel: RepoWheel, force: boolean) => void
}

function sizeText(bytes: number): string {
  if (!bytes) return '大小未知'
  if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(2)} GB`
  return `${(bytes / 1024 ** 2).toFixed(1)} MB`
}

/**
 * 点开包卡片后的页面：先选版本，再看详情，最后安装或保存。
 *
 * 安装过程仍交给 OperationDialog；这里只负责“选哪个轮子”和单个轮子的动作。
 */
export default function WheelPackageDialog({
  group,
  env,
  installBlocked,
  downloading,
  downloadError,
  onClose,
  onDownload,
  onInstall,
}: Props) {
  const dialogRef = useRef<HTMLDivElement>(null)
  const [selectedPath, setSelectedPath] = useState('')
  const [filters, setFilters] = useState<WheelFilters>(EMPTY_FILTERS)

  const options = useMemo(() => filterOptions(group.wheels, env), [group.wheels, env])
  const visible = useMemo(
    () =>
      [...group.compatible, ...group.incompatible].filter((wheel) =>
        wheelMatches(wheel, filters),
      ),
    [group.compatible, group.incompatible, filters],
  )
  const selected = useMemo(
    () => visible.find((wheel) => wheel.path === selectedPath) ?? visible[0] ?? null,
    [visible, selectedPath],
  )
  const hasFilters = Boolean(filters.cuda || filters.torch || filters.python)
  const canFilter = options.cuda.length + options.torch.length + options.python.length > 0
  const downloadingThis = Boolean(selected && downloading === selected.path)

  const applyEnv = () => {
    if (!env) return
    setFilters({
      cuda: options.cuda.length ? cudaDigits(env.torchCuda) : '',
      torch: options.torch.length ? torchMinor(env.torch) : '',
      python: options.python.length ? env.python.split('.').slice(0, 2).join('.') : '',
    })
  }

  useEffect(() => {
    const node = dialogRef.current
    if (!node) return
    const previous = document.activeElement as HTMLElement | null
    node.focus()
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('keydown', onKey)
      previous?.focus()
    }
  }, [onClose])

  const selectWheel = (wheel: RepoWheel) => {
    setSelectedPath(wheel.path)
  }

  return createPortal(
    <div
      className="modal-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose()
      }}
    >
      <div
        className="modal wheel-package-modal"
        role="dialog"
        aria-modal="true"
        aria-label={`${group.name} 轮子详情`}
        tabIndex={-1}
        ref={dialogRef}
      >
        <div className="modal-head">
          <span className={`wheel-detail-mark ${group.compatible.length ? 'is-ok' : 'is-warn'}`}>
            <Icon name={group.compatible.length ? 'check' : 'alert'} size={13} />
          </span>
          <div className="wheel-detail-title">
            <h2>{group.name}</h2>
            <span className="mono">
              {group.compatible.length} 个兼容 · 共 {group.wheels.length} 个轮子
            </span>
          </div>
          <div className="spacer" />
          <span className={`badge ${group.compatible.length ? 'ok' : 'idle'}`}>
            {group.compatible.length ? '有兼容版本' : '无兼容版本'}
          </span>
          <button className="icon-btn" onClick={onClose} aria-label="关闭">
            <Icon name="close" size={14} />
          </button>
        </div>

        <div className="modal-body wheel-package-body">
          <div className="wheel-pick-head">
            <div>
              <strong>选择轮子</strong>
              <span className="hint">先选一个版本，再安装或保存到本地</span>
            </div>
            <div className="spacer" />
            <span className="hint">
              {visible.length === group.wheels.length
                ? `${group.wheels.length} 个版本`
                : `${visible.length} / ${group.wheels.length} 个版本`}
            </span>
          </div>

          {canFilter && (
            <div className="wheel-pick-filters">
              {options.cuda.length > 0 && (
                <Select
                  className={`wheel-filter-select${filters.cuda ? ' is-on' : ''}`}
                  ariaLabel="按 CUDA 版本筛选"
                  value={filters.cuda}
                  options={[{ value: '', label: 'CUDA 全部' }, ...options.cuda]}
                  onChange={(value) => setFilters((prev) => ({ ...prev, cuda: value }))}
                />
              )}
              {options.torch.length > 0 && (
                <Select
                  className={`wheel-filter-select${filters.torch ? ' is-on' : ''}`}
                  ariaLabel="按 torch 版本筛选"
                  value={filters.torch}
                  options={[{ value: '', label: 'torch 全部' }, ...options.torch]}
                  onChange={(value) => setFilters((prev) => ({ ...prev, torch: value }))}
                />
              )}
              {options.python.length > 0 && (
                <Select
                  className={`wheel-filter-select${filters.python ? ' is-on' : ''}`}
                  ariaLabel="按 Python 版本筛选"
                  value={filters.python}
                  options={[{ value: '', label: 'Python 全部' }, ...options.python]}
                  onChange={(value) => setFilters((prev) => ({ ...prev, python: value }))}
                />
              )}
              <div className="wheel-filter-actions">
                {env && (
                  <button
                    type="button"
                    className="wheel-filter-link"
                    onClick={applyEnv}
                    title="按当前实例的 Python / torch / CUDA 筛选"
                  >
                    当前环境
                  </button>
                )}
                {hasFilters && (
                  <button
                    type="button"
                    className="wheel-filter-link"
                    onClick={() => setFilters(EMPTY_FILTERS)}
                  >
                    清除
                  </button>
                )}
              </div>
            </div>
          )}

          <div className="wheel-pick-list" role="listbox" aria-label="选择轮子版本">
            {visible.map((wheel) => {
              const active = selected?.path === wheel.path
              return (
                <button
                  type="button"
                  role="option"
                  aria-selected={active}
                  className={`wheel-pick-row${active ? ' is-active' : ''}${
                    wheel.compatible ? '' : ' is-incompatible'
                  }`}
                  key={wheel.path}
                  onClick={() => selectWheel(wheel)}
                >
                  <span className="wheel-pick-radio" aria-hidden />
                  <span className="wheel-pick-main">
                    <span className="wheel-pick-version mono">
                      {wheel.version || '未知版本'}
                      <em className={wheel.compatible ? 'is-ok' : 'is-warn'}>
                        {wheel.compatible ? '兼容' : '不兼容'}
                      </em>
                      {wheel.abiTag === 'abi3' && (
                        <em
                          className="wheel-pick-abi"
                          title={`abi3 稳定 ABI：Python ${pythonVersionOf(wheel.pythonTag) ?? '3.x'} 及以上都能用`}
                        >
                          abi3
                        </em>
                      )}
                    </span>
                    <span className="wheel-pick-file mono">{wheel.name}</span>
                  </span>
                  <span className="wheel-pick-size">{sizeText(wheel.size)}</span>
                </button>
              )
            })}
          </div>

          {visible.length === 0 && (
            <div className="wheel-pick-empty">
              {group.wheels.length === 0
                ? '这个包暂时没有可下载的轮子。'
                : '没有符合当前筛选的轮子，放宽条件或点「清除」再试。'}
            </div>
          )}

          {downloadError && <div className="banner err wheel-pick-feedback">{downloadError}</div>}
          {installBlocked && <div className="hint wheel-pick-feedback">{installBlocked}</div>}
        </div>

        <div className="modal-foot wheel-package-foot">
          <button
            className="ghost"
            disabled={!selected || downloadingThis}
            onClick={() => selected && onDownload(selected)}
          >
            <Icon name="download" size={13} />
            {downloadingThis ? '下载中…' : '下载到本地'}
          </button>
          <div className="spacer" />
          <button className="ghost" onClick={onClose}>
            取消
          </button>
          {selected && (
            <button
              className={selected.compatible ? 'primary' : 'danger'}
              disabled={Boolean(installBlocked)}
              onClick={() => onInstall(selected, !selected.compatible)}
            >
              {selected.compatible ? '安装' : '强制安装'}
            </button>
          )}
        </div>
      </div>
    </div>,
    document.body,
  )
}
