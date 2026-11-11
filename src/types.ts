export type EngineId = string

export type InstanceState =
  | 'uninitialized'
  | 'installing'
  | 'ready'
  | 'starting'
  | 'running'
  | 'stopping'
  | 'stopped'
  | 'updating'
  | 'error'

export interface AppInfo {
  name: string
  version: string
  os: string
  arch: string
  dataDir: string
  configPath: string
  instancesDir: string
  suggestedRoot: string
  dev: boolean
}

export interface Mirrors {
  pypi: string | null
  /** PyTorch 镜像根地址；安装时自动补当前引擎目录，留空用官方源 */
  torch: string | null
  hf: string | null
  githubProxy: string | null
}

export interface Paths {
  git: string | null
  /** 旧的单值共享模型目录：已由 storage.sharedModels 取代，只用于读旧配置 */
  modelsRoot: string
}

/** 一个模型目录：type 为空 = 整包 models 根目录，下面按子目录名展开 */
export interface ModelDir {
  path: string
  type: string | null
  /** 只有全局共享目录用得上：下载/保存默认落在哪个目录 */
  isDefault: boolean
}

/** 全局共享目录：模型可多个，输入/输出各只能一个（ComfyUI 自身限制） */
export interface StorageSettings {
  sharedModels: ModelDir[]
  sharedInput: string | null
  sharedOutput: string | null
  /** UV 下载 / 构建缓存上限（GB）；超过后自动清空，0 = 关闭 */
  uvCacheMaxGb: number
}

/** 输入/输出目录归属 */
export interface DirChoice {
  mode: 'own' | 'shared' | 'custom'
  path: string | null
}

export interface InstanceStorage {
  useSharedModels: boolean
  /** 总开关开着时，这个实例不要的共享目录（按路径记） */
  disabledSharedModels: string[]
  extraModels: ModelDir[]
  input: DirChoice
  output: DirChoice
}

/** 启动时真正生效的目录（后端算好后给界面看，所见即所得） */
export interface StorageDir {
  path: string
  /** own = 实例自己 | extra = 实例附加 | shared = 全局共享 */
  source: 'own' | 'extra' | 'shared' | string
  exists: boolean
  isDefault: boolean
  type: string | null
}

/** 某个全局共享目录在这个实例里的状态 */
export interface SharedModelOption {
  path: string
  type: string | null
  isDefault: boolean
  /** 这个实例用不用它 */
  applies: boolean
  exists: boolean
}

export interface StoragePlan {
  models: StorageDir[]
  input: StorageDir
  output: StorageDir
  yamlPath: string
  yamlText: string
  /** 全局设置里的共享输入/输出目录（可能还没填） */
  sharedInput: string | null
  sharedOutput: string | null
  /** 全局共享模型目录全量（含被这个实例关掉的），界面用勾选框列出来 */
  sharedModels: SharedModelOption[]
  extraArgs: string[]
  warnings: string[]
}

export interface StorageUpdateReq {
  id: string
  useSharedModels: boolean
  disabledSharedModels: string[]
  extraModels: ModelDir[]
  input: DirChoice
  output: DirChoice
}

/** 模型资源管理器里的一项；目录 size 为 0，modified 是毫秒时间戳 */
export interface ModelEntry {
  name: string
  isDir: boolean
  size: number
  modified: number | null
}

export interface ModelListing {
  path: string
  rel: string
  exists: boolean
  entries: ModelEntry[]
}

/** 拖文件进模型目录后的结果；copied/skipped/failed 都按文件数算 */
export interface ModelCopyResult {
  copied: number
  skipped: number
  failed: number
  bytes: number
  target: string
  errors: string[]
}

export interface InstallOpts {
  pypiFallback: boolean
  allowInstallScript: boolean
  autoSnapshot: boolean
}

export interface NetworkOpts {
  listenDefault: string
  portRange: [number, number]
  detectTimeoutMs: number
  /** HuggingFace 离线：只用本地缓存 */
  hfOffline: boolean
}

/** 一套完整的配色；暗色 / 亮色各存一份，切换主题不会互相覆盖。 */
export interface ThemePalette {
  accent: string
  bgTop: string
  bgMid: string
  bgBottom: string
  /** 卡片 / 面板底色；透明度另外统一控制 */
  cardColor: string
}

export interface UiOpts {
  theme: 'light' | 'dark'
  locale: 'zh-CN' | 'en-US'
  fontSize: number
  dark: ThemePalette
  light: ThemePalette
  /** 卡片透明度百分比：0 为不透明，80 为最透 */
  cardTransparency: number
}

export interface ActivityOpts {
  /** 单个实例的操作日志文件上限（字节） */
  maxBytes: number
}

/** 局域网同步的默认策略；每次预览里仍可临时改。 */
export interface SyncSettings {
  /** 预览里默认勾选「删除目标多余节点」 */
  removeExtraNodes: boolean
  /** 预览里默认勾选「删除目标多余模型」 */
  removeExtraModels: boolean
  /** 换掉旧 venv 后保留几天；0 = 不保留 */
  keepOldVenvDays: number
  /** 自动快照最多保留多少条；手动快照不自动删除 */
  snapshotMax: number
  /** 最近几次同步缓存（代次/收件箱/备份）保留多少组 */
  syncCacheKeep: number
}

export interface SyncCacheInfo {
  totalBytes: number
  generationBytes: number
  generationCount: number
  inboxBytes: number
  inboxCount: number
  backupBytes: number
  backupCount: number
  nodeCacheBytes: number
  nodeCacheCount: number
  hashCacheBytes: number
}

export interface UvCacheInfo {
  path: string
  totalBytes: number
}

/** 设置里的本地 wheel 库目录：用户自己把 `*.whl` 放进去。 */
export interface WheelsInfo {
  path: string
  count: number
  bytes: number
  /** 设置里填了目录（哪怕目录不存在）。 */
  configured: boolean
  /** 目录当前存在。 */
  exists: boolean
}

export interface UvCacheClear {
  freedBytes: number
  totalBytes: number
}

export interface GlobalSettings {
  version: number
  mirrors: Mirrors
  /** 云端 wheel 仓库地址（ModelScope 模型仓库） */
  wheelRepo: string | null
  /** 本地 wheel 库目录：用户手动安装的本地轮子放这里，恢复/同步时按名字取。 */
  wheels: string | null
  paths: Paths
  /** 全局共享目录：模型 / 输入 / 输出 */
  storage: StorageSettings
  install: InstallOpts
  network: NetworkOpts
  ui: UiOpts
  activity: ActivityOpts
  sync: SyncSettings
}

/** 一条操作日志：装节点、切版本、改参数、快照… */
export interface ActivityEntry {
  id: string
  /** 本地时间 RFC3339 */
  at: string
  /** start / stop / node-install / version-switch / launch-args / storage / snapshot… */
  kind: string
  level: 'ok' | 'info' | 'warn' | 'error'
  action: string
  detail: string | null
}

export interface ComfyRef {
  channel: 'stable' | 'dev'
  ref: string
  commit: string
  version: string | null
}

export interface ServerOpts {
  listen: string
  port: number
  corsOrigin: string | null
  extraArgs: string[]
}

export interface EngineDetail {
  torchIndexUrl: string
  torchVersion: string
  cudaTag: string | null
}

/** 云端实例：只记地址，不在本机装环境 */
export interface RemoteTarget {
  /** 域名或 IP（不带协议、不带端口） */
  host: string
  port: number
  /** true = https */
  tls: boolean
}

export interface InstanceConfig {  id: string
  name: string
  rootDir: string
  engine: string
  engineDetail: EngineDetail | null
  pythonVersion: string
  /** 这个实例用哪个 Python 环境（每个实例自己定） */
  python: PythonEnvConfig
  comfy: ComfyRef
  server: ServerOpts
  /** 模型 / 输入 / 输出目录怎么落 */
  storage: InstanceStorage
  env: Record<string, string>
  /** 实例级覆盖：null / 缺省 = 继承全局设置 */
  overrides: InstanceOverrides
  /** 节点依赖自动模式：off / remind（默认）/ auto */
  nodeDepsAuto: 'off' | 'remind' | 'auto' | null
  /** 进程非人为停止后自动重启 */
  autoRestart: boolean
  /** local = 本机实例；remote = 云端地址 */
  kind: 'local' | 'remote'
  /** 仅 kind = remote 时有值 */
  remote: RemoteTarget | null
  imported: boolean
  state: InstanceState
  createdAt: string
  lastStartedAt: string | null
}

/** 实例自己的 Python 环境：自建 venv（默认）或指向已有环境 */
export interface PythonEnvConfig {
  mode: 'managed' | 'existing'
  /** existing 时：venv 目录 / python.exe / 便携版 python_embeded；相对路径按实例根；空 = 在实例目录里自动找 */
  path: string | null
}

/** 检测一个路径能不能当 Python 环境（实例设置页「检测」用） */
export interface PythonProbe {
  python: string
  root: string
  version: string
  venvStyle: boolean
  managed: boolean
}

/** 从实例 venv 里实测到的真实环境（概览/设置页刷新用） */
export interface InstanceLogEvent {
  instanceId: string
  seq: number
  line: string
}

export interface InstanceLogState {
  lines: string[]
  seq: number
}

export interface LiveEnv {
  pythonVersion: string
  pythonVersionFull: string
  torchVersion: string
  cudaAvailable: boolean
  cudaDevice: string | null
  engine: string
  engineDetail: EngineDetail | null
  changed: boolean
}

/** 一个 Python 环境实测出来的信息 */
export interface EnvReport {
  python: string
  /** cp312 */
  abi: string
  /** win_amd64 / linux_x86_64 */
  platform: string
  torch: string | null
  /** 12.4 */
  torchCuda: string | null
  cxx11: boolean | null
  torchvision?: string | null
  torchaudio?: string | null
  torchcodec?: string | null
}

/** 云端仓库里的一个 wheel（按当前实例环境实时标记兼容性） */
export interface RepoWheel {
  name: string
  path: string
  group: string
  version: string
  size: number
  url: string
  label: string
  note: string | null
  compatible: boolean
  reasons: string[]
  /** 文件名里的 Python 标签：cp313 / py39 / py3 */
  pythonTag: string
  /** 文件名里的 ABI 标签：cp313 / abi3 / none */
  abiTag: string
  platformTag: string
  /** 文件名里的 torch 标签（去掉补丁号之前）：2.10 */
  torchTag: string | null
  /** 文件名里的 CUDA 标签：130 / 12.4 */
  cudaTag: string | null
}

export interface WheelRepoList {
  repoUrl: string
  env: EnvReport | null
  fetchedAt: number
  fromCache: boolean
  /** 刷新失败但保留了本地缓存时的错误；首次读取失败会直接抛错 */
  error: string | null
  total: number
  compatible: number
  wheels: RepoWheel[]
  hint: string | null
}

export interface InstanceOverrides {
  pypi: string | null
  git: string | null
  hf: string | null
  /** null = 继承全局 */
  hfOffline: boolean | null
}

/** 导入前读取 `.comfyfleet/profile.json` 的摘要（只读，不探测 torch） */
export interface ImportProfilePreview {
  root: string
  found: boolean
  valid: boolean
  name: string | null
  engine: string | null
  pythonVersion: string | null
  savedAt: string | null
  extraArgs: number
  envVars: number
  autoRestart: boolean
  nodeDepsAuto: string | null
  warnings: string[]
  error: string | null
}

/** 启动参数页：服务与网络 + 额外参数，只动 server 这一段 */
export interface LaunchUpdateReq {
  id: string
  listen: string
  port: number
  corsOrigin: string | null
  /** 一行一个参数，可以一行多个（--preview-method auto），空白按 argv 切 */
  extraArgs: string[]
}
export interface InstanceUpdateReq {
  id: string
  name: string
  engine: string
  pythonVersion: string
  python: PythonEnvConfig
  /** 不传 = 不改；设置页只更新自己拥有的字段 */
  channel?: 'stable' | 'dev'
  listen?: string
  port?: number
  corsOrigin?: string | null
  extraArgs?: string[]
  env: Record<string, string>
  overrides: InstanceOverrides
  /** 进程非人为停止后自动重启 */
  autoRestart: boolean
  /** 引擎 / Python 变更会重置环境，需显式确认 */
  allowEnvChange: boolean
}

export interface CreateInstanceReq {
  name: string
  rootDir: string
  engine: EngineId
  pythonVersion: string
  port: number
  channel: 'stable' | 'dev'
}

/** 添加 / 编辑云端实例；id 有值 = 编辑 */
export interface RemoteInstanceReq {
  id: string | null
  name: string
  host: string
  port: number
  tls: boolean
}

export type PtyEvent = { type: 'data'; b64: string } | { type: 'exit'; code: number }

/** 实例健康信息（来自 ComfyUI 的 /system_stats 与 /queue） */
export interface Health {
  vramUsedMib: number
  vramTotalMib: number
  /** GPU 利用率（0–100）：后端用 nvidia-smi 读，拿不到为 null */
  gpuUtilization?: number | null
  /** 系统内存（MiB）：ComfyUI 的 /system_stats 里带 */
  ramUsedMib: number
  ramTotalMib: number
  queueRunning: number
  queuePending: number
  /** 正在执行的任务；队列为空时不存在 */
  taskId?: string | null
  /** 可读的任务/工作流名称；后端从 workflow 元数据或输出节点推断 */
  taskName?: string | null
  updatedAt: string
}

/** 启动结果：实际使用的端口与访问地址 */
export interface StartOutcome {
  port: number
  url: string
}

/** ComfyUI 当前版本状态 */
export interface VersionInfo {
  current: string
  commit: string
  /** 分支名；停在 tag 上是 "HEAD" */
  branch: string
  dirty: boolean
  channel: string
  remote: string
}

/** 切版本前会清理的一个工作区文件 */
export interface VersionDirtyFile {
  /** git status 的两字符状态，如 M / ?? / R */
  status: string
  path: string
}

export interface TagInfo {
  name: string
  date: string
  current: boolean
}

/** 开发版（master）上的一条提交 */
export interface DevCommit {
  sha: string
  short: string
  date: string
  subject: string
  current: boolean
}

/** 版本/节点这类长任务的进度事件 */
export interface VersionEvent {
  stage: string
  message: string
  level: 'info' | 'warn' | 'error'
  done: boolean
  failed: boolean
}

/** 一次「检查应用更新」的结果 */
export interface AppUpdate {
  current: string
  latest: string | null
  notes: string | null
  pubDate: string | null
  available: boolean
  /** 正在运行的实例名：非空时更新会被后端拒绝，界面要先提示用户去停实例 */
  running: string[]
}

/** 应用自更新的下载/安装进度 */
export interface UpdateEvent {
  stage: string
  message: string
  downloaded: number
  total: number | null
  failed: boolean
  done: boolean
}

/** custom_nodes 下的一个节点 */
export interface NodeInfo {
  name: string
  enabled: boolean
  kind: 'git' | 'manual'
  url: string | null
  commit: string | null
  hasRequirements: boolean
  sizeMib: number
}

/** 体检结论 */
/** 一条依赖：原始 requirement 行 + 为什么 */
export interface DepLine {
  line: string
  reason: string
}

/** custom_nodes 里一个节点的依赖状态 */
export interface NodeDeps {
  name: string
  enabled: boolean
  hasRequirements: boolean
  hasInstallPy: boolean
  hasPyproject: boolean
  declared: number
  /** 要装 / 要调整的 */
  pending: DepLine[]
  /** 明确跳过的（torch 已装、文件里的 index 行…） */
  skipped: DepLine[]
  /** 只有真装时才执行、无法静态判断的（`-e`、直接链接） */
  manual: string[]
}

export interface DepsReport {
  noVenv: boolean
  pythonVersion: string
  packageCount: number
  checkedAt: string
  pendingNodes: number
  nodes: NodeDeps[]
  /** uv pip check 的冲突行 */
  conflicts: string[]
}

export interface DepsOptions {
  runInstallPy: boolean
}

export interface FailedLine {
  line: string
  error: string
}

export interface InstallPyResult {
  ran: boolean
  ok: boolean
  message: string
}

export interface NodeDepsResult {
  name: string
  installed: string[]
  skipped: DepLine[]
  failed: FailedLine[]
  installPy: InstallPyResult | null
}

export interface DepsInstallReport {
  snapshotId: string | null
  nodes: NodeDepsResult[]
  torchBefore: string | null
  torchAfter: string | null
  packagesBefore: number
  packagesAfter: number
  conflicts: string[]
  warnings: string[]
}
/** 一个节点有没有新版本（检查更新） */
export interface NodeUpdate {
  name: string
  /** 远端比本地多的提交数；0 = 已是最新（浅克隆下可能不准，界面只说「可更新」） */
  behind: number
  local: string | null
  remote: string | null
  /** 检查失败的原因；失败时仍允许手动更新 */
  error: string | null
}

export interface DiagIssue {
  level: 'error' | 'warn' | 'info'
  title: string
  detail: string
  suggestion: string
}

export interface DiagReport {
  pythonVersion: string
  torchVersion: string
  cudaAvailable: boolean
  gpuName: string
  packageCount: number
  checkedAt: string
  issues: DiagIssue[]
  pipCheck: string[]
}

/** 显卡探测结果 */
export interface GpuInfo {
  name: string
  driverVersion: string
  cudaVersion: string | null
  memoryMib: number | null
}

export type PythonStatus = 'verified' | 'supported' | 'untested'

export interface PythonOption {
  version: string
  status: PythonStatus
  note?: string | null
}

/** 一个可选的计算引擎档（由 engine-matrix.json 提供） */
export interface EngineOption {
  id: string
  label: string
  torchVersion: string
  cudaVersion: string | null
  /** torch 对应的全家桶版本（后端对照表给出，可能为 null） */
  torchvision: string | null
  torchaudio: string | null
  torchcodec: string | null
  indexUrl: string
  pythonSupport: string[]
  /** 驱动报告的 CUDA 是否满足该档要求（仅提示，不阻止安装） */
  driverOk: boolean
  driverNote: string | null
  verified: boolean
  note: string | null
}

/** 环境计划：这台机器能装什么、推荐什么 */
export interface EnvPlan {
  gpu: GpuInfo | null
  engines: EngineOption[]
  pythons: PythonOption[]
  recommendedEngine: string
  recommendedPython: string
  warnings: string[]
  matrixVersion: number
  matrixUpdatedAt: string
}

/** 当前生效的 git 来源 */
export interface GitInfo {
  path: string
  version: string
  /** bundled | custom | system */
  source: string
  bundledVersion: string
}

/** 安装进度事件（与 Rust install::InstallEvent 对应） */
export interface InstallEvent {
  step: string
  stepIndex: number
  stepTotal: number
  /** start | log | done | warn */
  phase: string
  message: string
}

export interface InstallReport {
  pythonVersion: string
  torchVersion: string
  cudaAvailable: boolean
  cudaDevice: string | null
  comfyDir: string
  engineId: string
  tag: string | null
}

/** 从实例卡片跳到控制台时携带的意图 */
export interface ConsoleTarget {
  instanceId: string
  instanceName: string
}

// ---------------------------------------------------------------- 快照

export interface SnapshotNode {
  name: string
  url: string | null
  commit: string | null
  enabled: boolean
  /** git 工作区有未提交改动（含未跟踪文件） */
  dirty: boolean
  /** full = 手工节点整目录归档；patch = 快照 commit 上叠加未提交改动；missing = 归档失败 */
  content: 'full' | 'patch' | 'missing' | null
  contentHash: string | null
}

export interface SnapshotCore {
  channel: string
  ref: string
  commit: string | null
  version: string | null
}

export interface SnapshotPython {
  version: string
  torch: string | null
}

/** 一条完整快照（详情页 / 恢复预览用） */
export interface Snapshot {
  version: number
  id: string
  createdAt: string
  trigger: string
  label: string | null
  comfyui: SnapshotCore
  python: SnapshotPython
  /** 启动参数；老快照没有这一项 */
  server: ServerOpts | null
  nodes: SnapshotNode[]
  packages: Record<string, string>
  note: string | null
}

export interface PkgDelta {
  name: string
  from: string
  to: string
}

export interface NodeDelta {
  name: string
  fromCommit: string | null
  toCommit: string | null
  fromEnabled: boolean
  toEnabled: boolean
  contentChanged: boolean
  fromDirty: boolean
  toDirty: boolean
}

/** 差异里的节点引用（新增/删除的行） */
export interface NodeRef {
  name: string
  commit: string | null
  enabled: boolean
}

export interface SnapshotDiffSummary {
  comfyuiChanged: boolean
  launchChanged: boolean
  nodesAdded: number
  nodesRemoved: number
  nodesChanged: number
  packagesAdded: number
  packagesRemoved: number
  packagesChanged: number
}

/** 两条快照之间的差异（时间线里就是「这条改了什么」） */
export interface SnapshotDiff {
  comfyuiChanged: boolean
  comfyuiFrom: string | null
  comfyuiTo: string | null
  launchChanged: boolean
  launchFrom: ServerOpts | null
  launchTo: ServerOpts | null
  nodesAdded: NodeRef[]
  nodesRemoved: NodeRef[]
  nodesChanged: NodeDelta[]
  packagesAdded: PkgDelta[]
  packagesRemoved: PkgDelta[]
  packagesChanged: PkgDelta[]
}

/** 时间线一行 */
export interface SnapshotSummary {
  id: string
  createdAt: string
  trigger: string
  label: string | null
  comfyuiVersion: string
  nodeCount: number
  packageCount: number
  note: string | null
  diff: SnapshotDiffSummary
  previousVersion: string | null
}

export interface SnapshotDetail {
  snapshot: Snapshot
  version: string
  diffPrevious: SnapshotDiff | null
  diffCurrent: SnapshotDiff
}

export interface ProtectedDrift {
  name: string
  target: string | null
  current: string | null
}

export interface RestoreTargets {
  coreCommit: string | null
  coreChanged: boolean
  launchChanged: boolean
  nodesInstall: number
  nodesSwitch: number
  nodesContent: number
  nodesRemove: number
  nodesEnable: number
  nodesDisable: number
  packagesInstall: number
  packagesRemove: number
  packagesProtected: ProtectedDrift[]
}

export interface RestorePreview {
  running: boolean
  diff: SnapshotDiff
  targets: RestoreTargets
  /** 这次要装、但 wheel 库里找不到文件的包；先让用户补齐或确认跳过 */
  missingWheels: MissingWheel[]
}

export interface RestoreEvent {
  step: string
  phase: 'start' | 'log' | 'done' | 'warn' | string
  message: string
  percent: number
}

export interface RestoreOptions {
  removeExtraNodes: boolean
  removeExtraPackages: boolean
}

export interface NameError {
  name: string
  error: string
}

export interface RestoreReport {
  targetId: string
  preSnapshot: string | null
  postSnapshot: string | null
  core: { changed: boolean; from: string | null; to: string | null; error: string | null }
  launch: { changed: boolean; from: ServerOpts | null; to: ServerOpts | null }
  nodes: {
    installed: string[]
    removed: string[]
    switched: string[]
    contentRestored: string[]
    enabled: string[]
    disabled: string[]
    failed: NameError[]
  }
  packages: {
    installed: string[]
    removed: string[]
    changed: PkgDelta[]
    protectedSkipped: ProtectedDrift[]
    failed: NameError[]
    leftover?: string[]
  }
  warnings: string[]
}
// ---------------------------------------------------------------- 局域网集群同步

export interface ClusterInstanceView {
  id: string
  name: string
  state: string
  coreVersion: string | null
  coreCommit: string | null
  pythonVersion: string
  nodeCount: number
  modelCount: number
  modelBytes: number
  /** 后端是否已采到节点/模型计数；false 时页面显示“—”，不要当成 0。 */
  countsKnown?: boolean
  diskFree: number
  lastApplied: LastApplied | null
}

export interface LastApplied {
  generationId: string
  sourceInstanceId: string
  sourceInstanceName: string
  appliedAt: string
  snapshotId: string | null
  fingerprint: string
}

export interface ClusterMemberView {
  id: string
  name: string
  hostname: string
  platform: string
  appVersion: string
  online: boolean
  lastSeen: string
  joinedAt: string
  modelStore: string
  instances: ClusterInstanceView[]
}

export interface ClusterView {
  id: string
  name: string
  port: number
  allowLan: boolean
  createdAt: string
  addresses: string[]
  members: ClusterMemberView[]
}

export interface MemberSelfView {
  clusterId: string
  clusterName: string
  leaderHost: string
  leaderPort: number
  memberId: string
  memberName: string
  online: boolean
  lastSeen: string
  instances: ClusterInstanceView[]
}

export interface SyncCoreDiff {
  changed: boolean
  fromCommit: string | null
  toCommit: string | null
  fromVersion: string | null
  toVersion: string | null
  fromChannel: string
  toChannel: string
  sourceDirty: boolean
}

export interface WheelDelta {
  name: string
  from: string
  to: string
  sha256: string | null
  available: boolean
  reason: string | null
  incompatible: boolean
  compatWarning?: string | null
}

export interface SyncPythonDiff {
  fromVersion: string
  toVersion: string
  fromTorch: string | null
  toTorch: string | null
  packagesAdded: PkgDelta[]
  packagesRemoved: PkgDelta[]
  packagesChanged: PkgDelta[]
  wheelsAdded: WheelDelta[]
  wheelsRemoved: WheelDelta[]
  wheelsChanged: WheelDelta[]
  missingWheels: WheelDelta[]
  incompatibleWheels?: WheelDelta[]
}

export interface SyncNodeChange {
  name: string
  fromCommit: string | null
  toCommit: string | null
  fromEnabled: boolean
  toEnabled: boolean
  contentChanged: boolean
  reason: string
}

export interface SyncNodeDiff {
  added: SyncNodeStateView[]
  removed: SyncNodeStateView[]
  changed: SyncNodeChange[]
  targetExtra: number
}

export interface SyncNodeStateView {
  name: string
  enabled: boolean
  kind: string
  url: string | null
  commit: string | null
  dirty: boolean
  hasLocalCommits: boolean
  worktreeHash: string
  archive: string | null
  archiveSha256: string | null
  archiveSize: number
}

export interface ModelChange {
  kind: string
  relativePath: string
  size: number
  fromSize: number
  fromHash: string | null
  toHash: string | null
}

export interface SyncModelDiff {
  added: ModelChange[]
  removed: ModelChange[]
  modified: ModelChange[]
  transferBytes: number
  reusableBytes: number
  unchanged: number
}

export interface SyncLaunchDiff {
  changed: boolean
  fromListen: string
  toListen: string
  fromPort: number
  toPort: number
  fromCorsOrigin: string | null
  toCorsOrigin: string | null
  fromExtraArgs: string[]
  toExtraArgs: string[]
  fromLocalExtraArgs: string[]
  toLocalExtraArgs: string[]
}

export interface SyncDiff {
  core: SyncCoreDiff
  launch: SyncLaunchDiff
  python: SyncPythonDiff
  nodes: SyncNodeDiff
  models: SyncModelDiff
  targetDrift: boolean
  targetLastSource: string | null
  warnings: string[]
  needsApply: boolean
}

export interface SyncTotals {
  files: number
  bytes: number
}

export interface SyncPreview {
  generationId: string
  sourceInstanceId: string
  sourceInstanceName: string
  targetMemberId: string
  targetInstanceId: string
  targetInstanceName: string
  targetOnline: boolean
  targetLastApplied: LastApplied | null
  targetFingerprint: string
  targetDiskFree: number
  diff: SyncDiff
  totals: SyncTotals
}

export interface SyncScope {
  comfyui: boolean
  python: boolean
  nodes: boolean
  models: boolean
}

export interface SyncOptions {
  removeExtraNodes: boolean
  removeExtraModels: boolean
  allowDirtyTarget: boolean
}

/** 目标端装不上的自定义依赖：缺 wheel，或目录/仓库不可用。 */
export interface MissingWheel {
  name: string
  version: string
  filename: string
  /** local-wheel / local-archive / local-dir / vcs / missing */
  source: string
  reason: string
}

/** sync_request 的结果：开始准备并发出任务，或先让用户处理缺的 wheel。 */
export type SyncRequestOutcome =
  | { outcome: 'started'; jobs: SyncJob[] }
  | { outcome: 'needsWheels'; missing: MissingWheel[]; library: string | null }

/** 源端准备阶段的实时状态；准备线程每处理一个文件就会更新 detail。 */
export interface SyncPrepare {
  sourceInstanceId: string
  phase: string
  percent: number
  message: string
  detail: string
  logs: string[]
  startedAt: string
  updatedAt: string
  done: boolean
  cancelled: boolean
  cancelling: boolean
  error: string | null
  generationId: string | null
  jobs: number
  files: number
  bytes: number
}

export interface SyncTarget {
  targetMemberId: string
  targetInstanceId: string
}

export interface SyncStartTarget extends SyncTarget {
  targetFingerprint: string
}

export interface SyncCompare {
  sourceInstanceId: string
  sourceInstanceName: string
  targetInstanceId: string
  targetInstanceName: string
  sourceRunning: boolean
  diff: SyncDiff
}

export interface SourceChanges {
  sourceInstanceId: string
  sourceInstanceName: string
  sourceRunning: boolean
  hasBaseline: boolean
  lastSyncAt: string | null
  lastSyncGeneration: string | null
  diff: SyncDiff
}

export interface SyncJob {
  id: string
  generationId: string
  sourceInstanceId: string
  sourceInstanceName: string
  targetMemberId: string
  targetMemberName?: string
  targetInstanceId: string
  targetInstanceName: string
  status: string
  createdAt: string
  acceptedAt?: string | null
  startedAt?: string | null
  finishedAt?: string | null
  updatedAt: string
  progress: number
  message: string
  error: string | null
  options: SyncOptions
  scope: SyncScope
  diff: SyncDiff
  targetFingerprint: string
  targetOnline: boolean
  cancelRequested?: boolean
  targetCancelRequested?: boolean
  targetCancelledAt?: string | null
  targetCancelCount?: number
  report: SyncReport | null
  logs: string[]
}

export interface SyncIssue {
  kind: string
  name: string
  reason: string
}

export interface SyncReport {
  preSnapshot: string | null
  postSnapshot: string | null
  core: string
  python: string
  nodes: number
  models: number
  warnings: string[]
  partial: boolean
  issues: SyncIssue[]
}

export interface ClusterStatus {
  role: 'none' | 'leader' | 'member'
  cluster: ClusterView | null
  member: MemberSelfView | null
  sources: ClusterInstanceView[]
  jobs: SyncJob[]
  addresses: string[]
  serverRunning: boolean
  agentRunning: boolean
  error: string | null
}

export interface CreateClusterReq {
  name: string
  password: string
  port: number
  allowLan: boolean
}

export interface JoinClusterReq {
  host: string
  port: number
  password: string
  memberName: string
}
