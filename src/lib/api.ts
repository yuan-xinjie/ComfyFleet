import { Channel, invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import type {
  ActivityEntry,
  AppInfo,
  AppUpdate,
  ClusterStatus,
  CreateClusterReq,
  CreateInstanceReq,
  JoinClusterReq,
  SyncCacheInfo,
  UvCacheClear,
  UvCacheInfo,
  WheelsInfo,
  SyncCompare,
  SyncJob,
  SyncOptions,
  SyncPrepare,
  SyncRequestOutcome,
  SyncScope,
  SourceChanges,
  SyncPreview,
  SyncStartTarget,
  SyncTarget,
  DepsInstallReport,
  DepsOptions,
  DepsReport,
  DevCommit,
  DiagReport,
  EnvPlan,
  GitInfo,
  GlobalSettings,
  Health,
  InstallEvent,
  InstallReport,
  ImportProfilePreview,
  InstanceConfig,
  InstanceLogEvent,
  InstanceLogState,
  InstanceUpdateReq,
  EnvReport,
  WheelRepoList,
  InstanceStorage,
  LaunchUpdateReq,
  LiveEnv,
  ModelCopyResult,
  ModelListing,
  NodeInfo,
  NodeUpdate,
  PtyEvent,
  PythonProbe,
  RemoteInstanceReq,
  RestoreEvent,
  RestoreOptions,
  RestorePreview,
  RestoreReport,
  SnapshotDetail,
  SnapshotSummary,
  StartOutcome,
  StoragePlan,
  StorageUpdateReq,
  TagInfo,
  UpdateEvent,
  VersionDirtyFile,
  VersionEvent,
  VersionInfo,
} from '../types'

export interface PtySpawnOpts {
  program: string
  args: string[]
  cwd: string | null
  cols: number
  rows: number
}

export interface NetSpeed {
  rxBps: number
  txBps: number
}

export function decodeBase64(b64: string): Uint8Array {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i)
  return out
}

export const api = {
  appInfo: () => invoke<AppInfo>('app_info'),
  /** 查应用自身有没有新版本；联网，失败时抛 { code, message } */
  appUpdateCheck: () => invoke<AppUpdate>('app_update_check'),
  /**
   * 下载并安装应用更新，装完自动重启。
   * 有实例在跑 / 正在装环境时后端会直接拒绝（INSTANCES_RUNNING / APP_BUSY）。
   */
  appUpdateInstall: (onEvent: (ev: UpdateEvent) => void): Promise<void> => {
    const channel = new Channel<UpdateEvent>()
    channel.onmessage = onEvent
    return invoke<void>('app_update_install', { onEvent: channel })
  },
  /** 本机实时网络吞吐；操作控制台运行中轮询 */
  netSpeed: () => invoke<NetSpeed>('net_speed'),
  openPath: (path: string) => invoke<void>('open_path', { path }),
  /** 打开原生文件夹选择框；取消返回 null */
  pickFolder: () => invoke<string | null>('pick_folder'),

  settingsGet: () => invoke<GlobalSettings>('settings_get'),
  /** 后端会归一化（去空目录、收敛非法值），返回落盘后的版本 */
  settingsSave: (settings: GlobalSettings) =>
    invoke<GlobalSettings>('settings_save', { settings }),
  /** 顶栏明暗切换：立即写盘并返回最新设置 */
  settingsSetTheme: (theme: 'light' | 'dark') =>
    invoke<GlobalSettings>('settings_set_theme', { theme }),
  syncCacheInfo: () => invoke<SyncCacheInfo>('sync_cache_info'),
  uvCacheInfo: () => invoke<UvCacheInfo>('uv_cache_info'),
  wheelsInfo: () => invoke<WheelsInfo>('wheels_info'),
  uvCacheClear: () => invoke<UvCacheClear>('uv_cache_clear'),
  setTrayIcon: (rgbaBase64: string) => invoke<void>('set_tray_icon', { rgbaBase64 }),

  instanceList: () => invoke<InstanceConfig[]>('instance_list'),
  instanceCreate: (req: CreateInstanceReq) => invoke<InstanceConfig>('instance_create', { req }),
  /** 添加 / 编辑云端实例：只存域名 / IP + 端口 */
  instanceRemoteSave: (req: RemoteInstanceReq) =>
    invoke<InstanceConfig>('instance_remote_save', { req }),
  instanceUpdate: (req: InstanceUpdateReq) =>
    invoke<InstanceConfig>('instance_update', { req }),
  /** 检测一个路径能不能当这个实例的 Python 环境（留空 = 按当前配置解析） */
  instancePythonProbe: (instanceId: string, path: string | null) =>
    invoke<PythonProbe | null>('instance_python_probe', { instanceId, path }),
  /** 只改服务与网络 / 启动参数（启动参数页用，不碰引擎、名称和环境变量） */
  instanceLaunchUpdate: (req: LaunchUpdateReq) =>
    invoke<InstanceConfig>('instance_launch_update', { req }),
  /** 启动时实际生效的目录（只读预览，不写盘） */
  /** draft = 界面上的草稿；给了就按草稿算，勾选框还没保存也能预览 */
  instanceStoragePlan: (id: string, draft?: InstanceStorage) =>
    invoke<StoragePlan>('instance_storage_plan', { id, draft: draft ?? null }),
  /** 只改存储（模型 / 输入 / 输出），不碰其它设置 */
  instanceStorageUpdate: (req: StorageUpdateReq) =>
    invoke<InstanceConfig>('instance_storage_update', { req }),
  /** 模型资源管理器：列当前实例某个生效模型目录（root + 相对路径） */
  modelsList: (instanceId: string, root: string, rel: string) =>
    invoke<ModelListing>('models_list', { instanceId, root, rel }),
  /** 把拖进来的文件/目录复制到当前模型目录；已存在的同名文件跳过 */
  modelsCopy: (instanceId: string, root: string, rel: string, sources: string[]) =>
    invoke<ModelCopyResult>('models_copy', { instanceId, root, rel, sources }),
  /** 用系统浏览器打开 http(s) 链接（官方文档之类） */
  openUrl: (url: string) => invoke<void>('open_url', { url }),
  instanceDelete: (id: string, purge: boolean) => invoke<void>('instance_delete', { id, purge }),
  /** 导入已有安装（M7）：只登记，不复制；有 .comfyfleet/profile.json 会自动恢复参数 */
  instanceImport: (path: string, name: string | null, pythonPath: string | null) =>
    invoke<InstanceConfig>('instance_import', { path, name, pythonPath }),
  /** 导入前预览目录里的参数文件；缺失/损坏只返回状态，不阻塞导入 */
  instanceImportProfile: (path: string) =>
    invoke<ImportProfilePreview>('instance_import_profile', { path }),
  /** 只改实例登记目录，不移动磁盘文件；新目录必须已有 ComfyUI/main.py */
  instanceRelocate: (id: string, rootDir: string) =>
    invoke<InstanceConfig>('instance_relocate', { req: { id, rootDir } }),
  instanceHealth: () => invoke<Record<string, Health>>('instance_health'),
  instanceStart: (instanceId: string) =>
    invoke<StartOutcome>('instance_start', { instanceId }),
  instanceStop: (instanceId: string) => invoke<void>('instance_stop', { instanceId }),
  /** 实际执行 venv 里的 python / import torch，重读 Python、torch、CUDA */
  instanceEnvRefresh: (instanceId: string) => invoke<LiveEnv>('instance_env_refresh', { instanceId }),
  /** 日志快照（尾部 + 序号）；实时增量由 onInstanceLog 推过来 */
  instanceLogState: (instanceId: string, lines?: number) =>
    invoke<InstanceLogState>('instance_log_state', { instanceId, lines }),
  onInstanceLog: (handler: (event: InstanceLogEvent) => void) =>
    listen<InstanceLogEvent>('instance-log', (event) => handler(event.payload)),
  instanceOpenUrl: (instanceId: string) => invoke<string>('instance_open_url', { instanceId }),

  /** M1 前置：仅用 uv 创建 venv（不装 torch / requirements） */
  envInit: (instanceId: string) => invoke<string>('env_init', { instanceId }),

  /** 环境计划：探测显卡/驱动，返回可用引擎档与推荐配置 */
  envPlan: () => invoke<EnvPlan>('env_plan'),

  /** 当前生效的 git（内置 MinGit / 自定义 / 系统） */
  gitInfo: () => invoke<GitInfo>('git_info'),

  /**
   * 安装环境：venv → ComfyUI 本体 → requirements → torch → triton → verify。
   * 引擎与 Python 取自 instance.json（不接受前端临时指定）。
   */
  envInstall: (instanceId: string, onEvent: (ev: InstallEvent) => void): Promise<InstallReport> => {
    const channel = new Channel<InstallEvent>()
    channel.onmessage = onEvent
    return invoke<InstallReport>('env_install', { instanceId, onEvent: channel })
  },

  /** 请求取消正在运行的环境安装；安装循环会终止当前子进程。 */
  envCancelInstall: (instanceId: string) =>
    invoke<void>('env_cancel_install', { instanceId }),

  ptySpawn: (opts: PtySpawnOpts, onEvent: (ev: PtyEvent) => void): Promise<string> => {
    const channel = new Channel<PtyEvent>()
    channel.onmessage = onEvent
    return invoke<string>('pty_spawn', { ...opts, onEvent: channel })
  },
  /** 进入实例的 venv 环境终端（cmd，注入 VIRTUAL_ENV / PATH / 镜像变量） */
  ptySpawnInstance: (
    instanceId: string,
    cols: number,
    rows: number,
    onEvent: (ev: PtyEvent) => void,
  ): Promise<string> => {
    const channel = new Channel<PtyEvent>()
    channel.onmessage = onEvent
    return invoke<string>('pty_spawn_instance', { instanceId, cols, rows, onEvent: channel })
  },
  ptyWrite: (id: string, data: string) => invoke<void>('pty_write', { id, data }),
  ptyResize: (id: string, cols: number, rows: number) =>
    invoke<void>('pty_resize', { id, cols, rows }),
  ptyKill: (id: string) => invoke<void>('pty_kill', { id }),

  // ---- M4 版本管理 ----
  versionInfo: (instanceId: string) =>
    invoke<VersionInfo>('version_info', { instanceId }),
  versionTags: (instanceId: string) =>
    invoke<TagInfo[]>('version_tags', { instanceId }),
  /** 切换/更新前会被清理的工作区文件；只读 */
  versionDirtyFiles: (instanceId: string) =>
    invoke<VersionDirtyFile[]>('version_dirty_files', { instanceId }),
  /** 开发版（master）最近的提交；需要联网，浅克隆会按需加深 */
  versionCommits: (instanceId: string, limit = 30) =>
    invoke<DevCommit[]>('version_commits', { instanceId, limit }),
  /** 与远端比对落后多少提交（联网）；null = 有更新但浅克隆数不清 */
  versionCheck: (instanceId: string) => invoke<number | null>('version_check', { instanceId }),
  versionCancel: (instanceId: string) => invoke<void>('version_cancel', { instanceId }),
  /** kind：tag（稳定版）/ branch（跟随开发版）/ commit（开发版的某个提交） */
  versionSwitch: (
    instanceId: string,
    target: string,
    kind: 'tag' | 'branch' | 'commit',
    onEvent: (ev: VersionEvent) => void,
    cleanWorkspace = false,
  ): Promise<VersionInfo> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<VersionInfo>('version_switch', {
      instanceId,
      target,
      kind,
      cleanWorkspace,
      onEvent: channel,
    })
  },
  versionUpdate: (
    instanceId: string,
    onEvent: (ev: VersionEvent) => void,
    cleanWorkspace = false,
  ): Promise<VersionInfo> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<VersionInfo>('version_update', { instanceId, cleanWorkspace, onEvent: channel })
  },

  // ---- M5 节点管理 ----
  nodesList: (instanceId: string) => invoke<NodeInfo[]>('nodes_list', { instanceId }),
  nodesInstall: (
    instanceId: string,
    url: string,
    name: string | null,
    installRequirements: boolean,
    onEvent: (ev: VersionEvent) => void,
  ): Promise<NodeInfo> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<NodeInfo>('nodes_install', {
      instanceId,
      url,
      name,
      installRequirements,
      onEvent: channel,
    })
  },
  nodesToggle: (instanceId: string, name: string, enabled: boolean) =>
    invoke<void>('nodes_toggle', { instanceId, name, enabled }),
  nodesUpdate: (
    instanceId: string,
    name: string,
    onEvent: (ev: VersionEvent) => void,
  ): Promise<NodeUpdate> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<NodeUpdate>('nodes_update', { instanceId, name, onEvent: channel })
  },
  nodesRemove: (instanceId: string, name: string, allowManual: boolean) =>
    invoke<void>('nodes_remove', { instanceId, name, allowManual }),

  /** 设置节点依赖自动模式（off / remind / auto） */
  instanceSetNodeAuto: (id: string, mode: 'off' | 'remind' | 'auto') =>
    invoke<InstanceConfig>('instance_set_node_auto', { id, mode }),

  /** 检查 git 节点在远端有没有新提交（fetch 到 FETCH_HEAD，不动工作区） */
  nodesCheckUpdates: (instanceId: string, names: string[] | null = null) =>
    invoke<NodeUpdate[]>('nodes_check_updates', { instanceId, names }),

  /** 节点依赖检测：读一次已装包表，逐节点算缺什么 */
  nodesDepsStatus: (instanceId: string) =>
    invoke<DepsReport>('nodes_deps_status', { instanceId }),
  /** 装依赖（`names` 为空 = 所有有依赖的节点）；装之前后端会自动拍快照 */
  nodesInstallDeps: (
    instanceId: string,
    names: string[] | null,
    options: DepsOptions,
    onEvent: (ev: VersionEvent) => void,
  ): Promise<DepsInstallReport> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<DepsInstallReport>('nodes_install_deps', {
      instanceId,
      names,
      options,
      onEvent: channel,
    })
  },

  /** 环境体检：读 python/torch/pip check，输出可读结论 */
  diagScan: (instanceId: string) => invoke<DiagReport>('diag_scan', { instanceId }),

  // ------------------------------------------------------------ 环境与轮子

  /** 导入前探一个目录 / 解释器（不写文件） */
  accelScan: (root: string, python: string | null) =>
    invoke<EnvReport>('accel_scan', { root, python }),

  /** 云端轮子仓库：默认读本地缓存；refresh=true 才重新访问 ModelScope */
  wheelRepoList: (instanceId: string, refresh = false) =>
    invoke<WheelRepoList>('wheel_repo_list', { instanceId, refresh }),

  /** 下载云端仓库里的 wheel 到系统下载目录（path 必须来自当前缓存列表） */
  wheelRepoDownload: (path: string, destDir: string) =>
    invoke<string>('wheel_repo_download', { path, destDir }),

  /** 安装云端仓库里的 wheel（path 必须来自当前缓存列表） */
  wheelRepoInstall: (
    instanceId: string,
    path: string,
    onEvent: (ev: VersionEvent) => void,
  ): Promise<EnvReport> => {
    const channel = new Channel<VersionEvent>()
    channel.onmessage = onEvent
    return invoke<EnvReport>('wheel_repo_install', { instanceId, path, onEvent: channel })
  },

  // ------------------------------------------------------------ 操作日志

  activityList: (instanceId: string, limit?: number) =>
    invoke<ActivityEntry[]>('activity_list', { instanceId, limit: limit ?? null }),

  // ------------------------------------------------------------ 快照

  snapshotList: (instanceId: string) =>
    invoke<SnapshotSummary[]>('snapshot_list', { instanceId }),

  snapshotDetail: (instanceId: string, id: string) =>
    invoke<SnapshotDetail>('snapshot_detail', { instanceId, id }),

  snapshotCreate: (instanceId: string, label: string | null) =>
    invoke<SnapshotSummary>('snapshot_create', { instanceId, label }),

  snapshotDelete: (instanceId: string, id: string) =>
    invoke<void>('snapshot_delete', { instanceId, id }),

  snapshotRestorePreview: (instanceId: string, id: string) =>
    invoke<RestorePreview>('snapshot_restore_preview', { instanceId, id }),

  snapshotRestore: (
    instanceId: string,
    id: string,
    options: RestoreOptions,
    onEvent: (ev: RestoreEvent) => void,
  ): Promise<RestoreReport> => {
    const channel = new Channel<RestoreEvent>()
    channel.onmessage = onEvent
    return invoke<RestoreReport>('snapshot_restore', {
      instanceId,
      id,
      options,
      onEvent: channel,
    })
  },

  // ------------------------------------------------------------ 局域网集群

  clusterStatus: () => invoke<ClusterStatus>('cluster_status'),
  clusterRetryServer: () => invoke<ClusterStatus>('cluster_retry_server'),
  clusterSetLan: (allowLan: boolean) => invoke<ClusterStatus>('cluster_set_lan', { allowLan }),
  clusterCreate: (req: CreateClusterReq) => invoke<ClusterStatus>('cluster_create', { req }),
  clusterJoin: (req: JoinClusterReq) => invoke<ClusterStatus>('cluster_join', { req }),
  clusterLeave: () => invoke<void>('cluster_leave'),
  clusterDelete: (password: string) => invoke<void>('cluster_delete', { password }),
  clusterRemoveMember: (memberId: string) =>
    invoke<ClusterStatus>('cluster_remove_member', { memberId }),
  clusterCancelJob: (jobId: string) =>
    invoke<ClusterStatus>('cluster_cancel_job', { jobId }),
  clusterDeleteJob: (jobId: string) =>
    invoke<ClusterStatus>('cluster_delete_job', { jobId }),
  clusterCancelMemberJob: (jobId: string) =>
    invoke<ClusterStatus>('cluster_cancel_member_job', { jobId }),
  clusterRetryMemberJob: (jobId: string) =>
    invoke<ClusterStatus>('cluster_retry_member_job', { jobId }),
  clusterConfirmJob: (jobId: string, targetInstanceId: string, options: SyncOptions) =>
    invoke<ClusterStatus>('cluster_confirm_job', { jobId, targetInstanceId, options }),
  clusterRejectJob: (jobId: string) =>
    invoke<ClusterStatus>('cluster_reject_job', { jobId }),
  clusterRefresh: () => invoke<ClusterStatus>('cluster_refresh'),

  clusterCompare: (sourceInstanceId: string, targetInstanceId: string) =>
    invoke<SyncCompare>('cluster_compare', { sourceInstanceId, targetInstanceId }),

  clusterSourceChanges: (sourceInstanceId: string) =>
    invoke<SourceChanges>('cluster_source_changes', { sourceInstanceId }),
  syncRequest: (
    sourceInstanceId: string,
    targetMemberIds: string[],
    scope: SyncScope,
    skipMissingWheels = false,
  ) =>
    invoke<SyncRequestOutcome>('sync_request', {
      req: { sourceInstanceId, targetMemberIds, scope, skipMissingWheels },
    }),
  syncPrepareStatus: (sourceInstanceId: string) =>
    invoke<SyncPrepare | null>('sync_prepare_status', { sourceInstanceId }),
  syncPrepareCancel: (sourceInstanceId: string) =>
    invoke<void>('sync_prepare_cancel', { sourceInstanceId }),
  syncPrepareClear: (sourceInstanceId: string) =>
    invoke<void>('sync_prepare_clear', { sourceInstanceId }),
  syncPreview: (
    sourceInstanceId: string,
    targetMemberId: string,
    targetInstanceId: string,
  ) =>
    invoke<SyncPreview>('sync_preview', {
      req: { sourceInstanceId, targetMemberId, targetInstanceId },
    }),
  syncPreviewBatch: (sourceInstanceId: string, targets: SyncTarget[]) =>
    invoke<SyncPreview[]>('sync_preview_batch', {
      req: { sourceInstanceId, targets },
    }),
  syncStartBatch: (
    generationId: string,
    targets: SyncStartTarget[],
    options: SyncOptions,
  ) =>
    invoke<SyncJob[]>('sync_start_batch', {
      req: { generationId, targets, options },
    }),
  syncStart: (
    generationId: string,
    targetMemberId: string,
    targetInstanceId: string,
    targetFingerprint: string,
    options: SyncOptions,
  ) =>
    invoke<SyncJob>('sync_start', {
      req: {
        generationId,
        targetMemberId,
        targetInstanceId,
        targetFingerprint,
        options,
      },
    }),
  syncJob: (jobId: string) => invoke<SyncJob | null>('sync_job', { jobId }),
  syncJobs: () => invoke<SyncJob[]>('sync_jobs'),
}

export function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e && typeof e === 'object') {
    const anyErr = e as { message?: string; code?: string }
    if (anyErr.message) return anyErr.code ? `${anyErr.code}: ${anyErr.message}` : anyErr.message
  }
  return String(e)
}
