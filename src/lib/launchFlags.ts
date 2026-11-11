/**
 * ComfyUI 启动参数字典（`main.py` 的命令行参数）。
 *
 * 参数名与语义对着官方文档的「启动参数」页
 * （https://docs.comfy.org/zh/development/comfyui-server/startup-flags），
 * 以及 `ComfyUI/comfy/cli_args.py`（v0.43.1）里的 argparse 定义逐项核对过。
 * 可安全组合的官方参数都列在这里；以下类型故意不列：
 * - listen / port / CORS / input/output/extra_model_paths 由页面字段或「存储」页生成；
 * - TLS 两个参数要求启动器把健康检查和打开地址都改成 https，当前不支持；
 * - CI 快速测试、列出 feature flag 这类“执行完就退出”的参数；
 * - 已弃用的 --disable-api-nodes 和未进文档的 --enable-agent。
 * 其它冷门参数仍可写进「自定义参数」，这里不猜语义。
 *
 * `exclusive` 对应 cli_args.py 里的 add_mutually_exclusive_group：同组只能有一个，
 * 否则 argparse 直接报 not allowed with argument，实例起不来。
 */

export interface FlagValue {
  /** 文档里的默认值；勾选时直接填这个，用户可以改 */
  def: string
  /** 有可选值时为枚举（渲染成下拉） */
  choices?: string[]
  numeric?: boolean
  placeholder?: string
  /** 值留空 ComfyUI 会拒绝启动 */
  required?: boolean
}

export interface FlagSpec {
  flag: string
  help: string
  value?: FlagValue
  exclusive?: string
  /** 会影响输出质量或速度，界面上标个提示色 */
  risky?: boolean
}

export interface FlagGroup {
  id: string
  title: string
  note?: string
  flags: FlagSpec[]
}

export const FLAG_GROUPS: FlagGroup[] = [
  {
    id: 'vram',
    title: '显存与设备',
    note: '不勾任何一项 = 默认的动态显存',
    flags: [
      {
        flag: '--gpu-only',
        help: '模型和文本编码器全部放显存，最激进',
        exclusive: 'vram',
        risky: true,
      },
      {
        flag: '--highvram',
        help: '用完不卸载，模型一直留在显存里（显存足够时更快）',
        exclusive: 'vram',
      },
      {
        flag: '--lowvram',
        help: '文本编码器放 CPU，显存吃紧时最常用的省显存手段（动态显存开启时无效）',
        exclusive: 'vram',
      },
      {
        flag: '--novram',
        help: 'lowvram 还不够时用，几乎全部卸载到内存，很慢',
        exclusive: 'vram',
        risky: true,
      },
      {
        flag: '--cpu',
        help: '全部用 CPU 跑，极慢，只用来排查是不是显卡的问题',
        exclusive: 'vram',
        risky: true,
      },
      {
        flag: '--reserve-vram',
        help: '给系统和其他软件预留多少 GB 显存（默认按操作系统自动留）',
        value: { def: '2', numeric: true, placeholder: '2', required: true },
      },
      {
        flag: '--vram-headroom',
        help: '动态显存额外保持空闲的显存 GB，连别的程序占的也算在内',
        value: { def: '1', numeric: true, placeholder: '1', required: true },
      },
      {
        flag: '--disable-smart-memory',
        help: '更积极地把模型卸载到内存，不在显存里留缓存',
      },
      {
        flag: '--disable-nvml-pressure',
        help: '动态显存改用 CUDA 而不是 NVML 估算内存压力',
      },
      {
        flag: '--fast-disk',
        help: '动态加载/卸载改走磁盘（NVMe 快的时候可能更快）',
        exclusive: 'disk',
      },
      {
        flag: '--disable-fast-disk',
        help: '不要自动走磁盘，改回未锁页内存（覆盖 --fast-disk）',
        exclusive: 'disk',
      },
      {
        flag: '--disable-pinned-memory',
        help: '关闭 pinned memory（锁页内存）',
      },
      {
        flag: '--disable-cuda-graphs',
        help: '关闭 CUDA graphs（个别显卡/自定义节点会因此更稳）',
      },
      {
        flag: '--enable-dynamic-vram',
        help: '强制启用动态显存（较新 N 卡默认已启用，部分系统需手动开）',
        exclusive: 'dynamic_vram',
      },
      {
        flag: '--disable-dynamic-vram',
        help: '关闭动态显存，改回按估算加载（省内存时可能更慢）',
        exclusive: 'dynamic_vram',
      },
      {
        flag: '--disable-comfy-compiler',
        help: '关闭 Comfy 模型编译器，连它带的 CUDA graph 一起关',
      },
      {
        flag: '--async-offload',
        help: '异步权重卸载，数字是卸载流数量（N 卡默认已开，值留空=2）',
        value: { def: '', numeric: true, placeholder: '2' },
      },
      {
        flag: '--disable-async-offload',
        help: '关闭异步权重卸载',
      },
      {
        flag: '--cuda-device',
        help: '只用指定 CUDA 设备：0、0,1，或 all（默认在 Windows 上只用 0 号）',
        value: { def: '0', placeholder: '0', required: true },
      },
      {
        flag: '--default-device',
        help: '默认设备编号，其他设备仍然可见（0 是第一个）',
        value: { def: '0', numeric: true, placeholder: '0' },
      },
      {
        flag: '--cuda-malloc',
        help: '启用 cudaMallocAsync（torch 2.0+ 默认已启用）',
        exclusive: 'malloc',
      },
      {
        flag: '--disable-cuda-malloc',
        help: '关闭 cudaMallocAsync（个别驱动或显卡更稳）',
        exclusive: 'malloc',
      },
      {
        flag: '--directml',
        help: '改用 torch-directml（Windows + AMD/Intel，需先装 torch-directml）',
        value: { def: '', numeric: true, placeholder: '0（留空 = -1）' },
        risky: true,
      },
      {
        flag: '--oneapi-device-selector',
        help: 'Intel oneAPI 设备选择器，例如 GPU.0 / GPU.1',
        value: { def: 'GPU.0', placeholder: 'GPU.0', required: true },
        risky: true,
      },
      {
        flag: '--force-non-blocking',
        help: '强制张量使用非阻塞操作；部分非 N 卡可能更快，也可能让个别工作流异常',
        risky: true,
      },
      {
        flag: '--mmap-torch-files',
        help: '加载 ckpt/pt 时用 mmap，减少一次性内存占用',
      },
      {
        flag: '--disable-mmap',
        help: '加载 safetensors 时不用 mmap（排查内存映射问题时用）',
      },
    ],
  },
  {
    id: 'precision',
    title: '精度',
    note: '改精度会影响画质和显存占用',
    flags: [
      {
        flag: '--force-fp32',
        help: '全局强制 fp32（更慢、更占显存）',
        exclusive: 'fp',
        risky: true,
      },
      {
        flag: '--force-fp16',
        help: '全局强制 fp16（同时把 UNET 也设成 fp16）',
        exclusive: 'fp',
      },
      {
        flag: '--fp32-unet',
        help: '扩散模型（UNET）用 fp32 跑，最稳但最占显存',
        exclusive: 'fpunet',
        risky: true,
      },
      {
        flag: '--fp64-unet',
        help: '扩散模型用 fp64 跑，极慢，只用于排查数值问题',
        exclusive: 'fpunet',
        risky: true,
      },
      {
        flag: '--bf16-unet',
        help: '扩散模型用 bf16 跑',
        exclusive: 'fpunet',
      },
      {
        flag: '--fp16-unet',
        help: '扩散模型用 fp16 跑',
        exclusive: 'fpunet',
      },
      {
        flag: '--fp8_e4m3fn-unet',
        help: '扩散模型权重用 fp8 e4m3fn 存（Ada 及更新 N 卡更合适）',
        exclusive: 'fpunet',
      },
      {
        flag: '--fp8_e5m2-unet',
        help: '扩散模型权重用 fp8 e5m2 存（范围更大、精度更低）',
        exclusive: 'fpunet',
      },
      {
        flag: '--fp8_e8m0fnu-unet',
        help: '扩散模型权重用 fp8 e8m0fnu 存（主要表示 2 的幂缩放）',
        exclusive: 'fpunet',
        risky: true,
      },
      {
        flag: '--fp16-vae',
        help: 'VAE 用 fp16 跑，可能出黑图',
        exclusive: 'fpvae',
        risky: true,
      },
      {
        flag: '--bf16-vae',
        help: 'VAE 用 bf16 跑',
        exclusive: 'fpvae',
      },
      {
        flag: '--fp32-vae',
        help: 'VAE 用完整 fp32 跑（最稳，最占显存）',
        exclusive: 'fpvae',
      },
      {
        flag: '--cpu-vae',
        help: 'VAE 放到 CPU 上跑（省显存，慢）',
      },
      {
        flag: '--fp8_e4m3fn-text-enc',
        help: '文本编码器权重用 fp8 e4m3fn 存',
        exclusive: 'fpte',
      },
      {
        flag: '--fp8_e5m2-text-enc',
        help: '文本编码器权重用 fp8 e5m2 存（范围更大、精度更低）',
        exclusive: 'fpte',
      },
      {
        flag: '--fp16-text-enc',
        help: '文本编码器权重用 fp16 存',
        exclusive: 'fpte',
      },
      {
        flag: '--bf16-text-enc',
        help: '文本编码器权重用 bf16 存',
        exclusive: 'fpte',
      },
      {
        flag: '--fp32-text-enc',
        help: '文本编码器权重用 fp32 存，最稳、最占显存',
        exclusive: 'fpte',
      },
      {
        flag: '--fp16-intermediates',
        help: '节点之间的中间张量用 fp16（实验性）',
        risky: true,
      },
      {
        flag: '--force-channels-last',
        help: '推理时强制 channels-last 内存格式',
      },
      {
        flag: '--supports-fp8-compute',
        help: '假装设备支持 fp8 计算（硬件不支持时可能崩溃或出图异常）',
        risky: true,
      },
      {
        flag: '--enable-triton-backend',
        help: '启用 comfy-kitchen 的 Triton 后端（默认关闭，实验性）',
        exclusive: 'triton',
        risky: true,
      },
      {
        flag: '--disable-triton-backend',
        help: '强制关闭 comfy-kitchen 的 Triton 后端（覆盖启用参数）',
        exclusive: 'triton',
      },
    ],
  },
  {
    id: 'cache',
    title: '缓存',
    note: '默认是按内存压力自动缓存',
    flags: [
      {
        flag: '--cache-none',
        help: '不缓存节点结果，省内存/显存，但每次都要重算每个节点',
        exclusive: 'cache',
        risky: true,
      },
      {
        flag: '--cache-classic',
        help: '老式的激进缓存',
        exclusive: 'cache',
      },
      {
        flag: '--cache-lru',
        help: 'LRU 缓存，最多缓存 N 个节点结果（可能更吃内存/显存）',
        value: { def: '100', numeric: true, placeholder: '100', required: true },
        exclusive: 'cache',
      },
      {
        flag: '--high-ram',
        help: '内存很大、宁愿用页面文件也不反复加载模型时用',
        exclusive: 'cache',
      },
      {
        flag: '--cache-ram',
        help: '按内存压力缓存：可填两个 GB 值（活跃阈值 / 非活跃阈值），留空=默认',
        value: { def: '', placeholder: '8 64' },
        exclusive: 'cache',
      },
    ],
  },
  {
    id: 'preview',
    title: '采样预览',
    flags: [
      {
        flag: '--preview-method',
        help: '采样时中间预览的生成方式（none 最省性能）',
        value: {
          def: 'auto',
          choices: ['none', 'auto', 'latent2rgb', 'taesd'],
          required: true,
        },
      },
      {
        flag: '--preview-size',
        help: '预览图的最大尺寸（像素，默认 512）',
        value: { def: '512', numeric: true, placeholder: '512' },
      },
    ],
  },
  {
    id: 'attention',
    title: '注意力',
    note: '用哪个注意力后端由 ComfyUI 自己判断，没装对应库时它会自己退回',
    flags: [
      {
        flag: '--use-sage-attention',
        help: '用 SageAttention（需自己装好）',
        exclusive: 'attn',
      },
      {
        flag: '--use-flash-attention',
        help: '用 FlashAttention（需自己装好）',
        exclusive: 'attn',
      },
      {
        flag: '--use-pytorch-cross-attention',
        help: '用 PyTorch 2.0 自带的 cross attention',
        exclusive: 'attn',
      },
      {
        flag: '--use-quad-cross-attention',
        help: '用 sub-quadratic cross attention（省显存老方案，装 xformers 时无效）',
        exclusive: 'attn',
      },
      {
        flag: '--use-ck-attention',
        help: '用 Comfy Kitchen attention（需要硬件支持它的 INT8 内核）',
        exclusive: 'attn',
        risky: true,
      },
      {
        flag: '--use-split-cross-attention',
        help: '分块 cross attention（省显存的老方案，装了 xformers 时无效）',
        exclusive: 'attn',
      },
      {
        flag: '--disable-xformers',
        help: '不用 xformers',
      },
      {
        flag: '--force-upcast-attention',
        help: '强制 attention 上采样（官方说法：如果它能修好黑图，请反馈）',
        exclusive: 'upcast',
        risky: true,
      },
      {
        flag: '--dont-upcast-attention',
        help: '完全关闭 attention 的上采样（除调试外基本不需要）',
        exclusive: 'upcast',
        risky: true,
      },
    ],
  },
  {
    id: 'manager',
    title: 'ComfyUI Manager',
    note: '0.43 起 Manager 是核心功能；按需开启或换成旧界面',
    flags: [
      {
        flag: '--enable-manager',
        help: '启用 ComfyUI Manager',
      },
      {
        flag: '--disable-manager-ui',
        help: '只关掉 Manager 的界面和接口，后台定时任务仍会跑（需要同时启用 Manager）',
        exclusive: 'manager_ui',
      },
      {
        flag: '--enable-manager-legacy-ui',
        help: '使用 Manager 旧界面，会自动启用 Manager',
        exclusive: 'manager_ui',
      },
    ],
  },
  {
    id: 'performance',
    title: '性能与调试',
    note: '实验性优化和排错开关；出问题时先关掉这里',
    flags: [
      {
        flag: '--fast',
        help: '实验性优化；勾选但不填值 = 全部开启，也可以填 fp16_accumulation、fp8_matrix_mult、cublas_ops、autotune',
        value: { def: '', placeholder: 'fp16_accumulation cublas_ops' },
        risky: true,
      },
      {
        flag: '--deterministic',
        help: '尽量使用确定性的 PyTorch 算法（更慢，不保证所有图完全一致）',
      },
      {
        flag: '--default-hashing-function',
        help: '比较重复文件名/内容时用的哈希算法（影响缓存和去重，不改画质）',
        value: {
          def: 'sha256',
          choices: ['md5', 'sha1', 'sha256', 'sha512'],
          required: true,
        },
      },
      {
        flag: '--debug-hang',
        help: '按 Ctrl+C 时输出堆栈，排查卡死',
        risky: true,
      },
      {
        flag: '--assert-graph-breaks',
        help: 'Comfy 模型编译器遇到 graph break 时直接失败（排查编译问题时用）',
        risky: true,
      },
    ],
  },
  {
    id: 'ui',
    title: '界面、日志与其他',
    flags: [
      {
        flag: '--dont-print-server',
        help: '不在终端打印服务器输出（日志会少很多）',
      },
      {
        flag: '--disable-product-tips',
        help: '隐藏终端里的商业产品提示',
      },
      {
        flag: '--verbose',
        help: '日志级别；不给值等于 DEBUG',
        value: {
          def: 'DEBUG',
          choices: ['DEBUG', 'DETAIL', 'INFO', 'WARNING', 'ERROR', 'CRITICAL'],
        },
      },
      {
        flag: '--offline',
        help: '让前端彻底断网（同时禁用付费 API 节点）',
        risky: true,
      },
      {
        flag: '--disable-partner-nodes',
        help: '只禁用付费 API 节点，前端照常联网',
      },
      {
        flag: '--disable-metadata',
        help: '不把工作流元数据写进输出图片/文件',
      },
      {
        flag: '--multi-user',
        help: '按用户隔离存储（多用户共用一台机器时用）',
      },
      {
        flag: '--disable-all-custom-nodes',
        help: '不加载任何自定义节点（排查节点冲突的最快办法）',
        risky: true,
      },
      {
        flag: '--whitelist-custom-nodes',
        help: '配合上面的禁用开关，只允许这些自定义节点文件夹（空格分隔）',
        value: { def: '', placeholder: 'ComfyUI-Manager comfyui-impact-pack', required: true },
      },
      {
        flag: '--disabled-nodes-config',
        help: 'YAML 文件路径，里面列要禁用的节点 ID',
        value: { def: '', placeholder: 'disabled_nodes.yaml', required: true },
      },
      {
        flag: '--log-stdout',
        help: '把正常输出发到 stdout，而不是默认的 stderr（接日志采集时用）',
      },
      {
        flag: '--auto-launch',
        help: 'ComfyUI 启动后自动再开一次浏览器；启动器里也有打开按钮，可能多一个标签页',
        exclusive: 'autolaunch',
      },
      {
        flag: '--disable-auto-launch',
        help: '禁止 ComfyUI 自己开浏览器（不影响启动器的打开按钮）',
        exclusive: 'autolaunch',
      },
      {
        flag: '--windows-standalone-build',
        help: 'Windows 便携版便捷模式：自动开浏览器 + 启用资产系统；启动器下通常不需要',
        risky: true,
      },
    ],
  },
  {
    id: 'frontend',
    title: '前端、API 与数据（进阶）',
    note: '会改变前端来源、Comfy API 或数据库；拿不准就用默认，改错可能启动失败',
    flags: [
      {
        flag: '--front-end-version',
        help: '指定前端版本，格式 owner/repo@version；需要联网下载，可能覆盖内置前端',
        value: { def: 'comfyanonymous/ComfyUI@latest', placeholder: 'comfyanonymous/ComfyUI@latest', required: true },
        risky: true,
      },
      {
        flag: '--front-end-root',
        help: '本地前端目录，覆盖 --front-end-version（路径必须存在且可读）',
        value: { def: '', placeholder: 'D:\\ComfyUI\\web_custom', required: true },
        risky: true,
      },
      {
        flag: '--comfy-api-base',
        help: 'Comfy API 基础地址，默认 https://api.comfy.org',
        value: { def: 'https://api.comfy.org', placeholder: 'https://api.comfy.org', required: true },
        risky: true,
      },
      {
        flag: '--database-url',
        help: '数据库地址，例如 sqlite:///:memory:；改错会丢历史/工作流',
        value: { def: '', placeholder: 'sqlite:///D:\\ComfyUI\\user\\comfyui.db', required: true },
        risky: true,
      },
      {
        flag: '--enable-assets',
        help: '启用资产系统（API、数据库同步、后台扫描）',
        exclusive: 'assets',
      },
      {
        flag: '--disable-assets',
        help: '即使开了资产系统也强制关闭（优先级更高）',
        exclusive: 'assets',
      },
      {
        flag: '--enable-asset-hashing',
        help: '扫描资产时计算 blake3 哈希；需要先启用资产系统，大型模型目录会明显变慢',
      },
      {
        flag: '--feature-flag',
        help: '设置一个服务端功能开关，如 show_signin_button=true；多个请写自定义参数',
        value: { def: '', placeholder: 'show_signin_button=true', required: true },
      },
    ],
  },
  {
    id: 'paths',
    title: '目录与上传（进阶）',
    note: '路径写错 ComfyUI 会直接启动失败，改完先看上面的启动命令预览',
    flags: [
      {
        flag: '--models-directory',
        help: '模型目录，覆盖 ComfyUI/models',
        value: { def: '', placeholder: 'D:\\models', required: true },
      },
      {
        flag: '--base-directory',
        help: '总基础目录，同时改 models/custom_nodes/input/output/temp/user；慎用，可能和启动器存储布局冲突',
        value: { def: '', placeholder: 'D:\\ComfyUI-base', required: true },
        risky: true,
      },
      {
        flag: '--temp-directory',
        help: '临时目录，覆盖 ComfyUI/temp',
        value: { def: '', placeholder: 'D:\\temp', required: true },
      },
      {
        flag: '--previews-directory',
        help: '资产预览图目录，覆盖默认位置',
        value: { def: '', placeholder: 'D:\\previews', required: true },
      },
      {
        flag: '--user-directory',
        help: '用户目录（绝对路径，必须已存在且可读）；存放设置、工作流和数据库',
        value: { def: '', placeholder: 'D:\\ComfyUI\\user', required: true },
      },
      {
        flag: '--output-directory',
        help: '输出目录，覆盖 ComfyUI/output',
        value: { def: '', placeholder: 'D:\\output', required: true },
      },
      {
        flag: '--extra-model-paths-config',
        help: '额外的 extra_model_paths.yaml 路径',
        value: { def: '', placeholder: 'extra_model_paths.yaml', required: true },
      },
      {
        flag: '--max-upload-size',
        help: '单次上传大小上限（MB，默认 100）',
        value: { def: '100', numeric: true, placeholder: '100', required: true },
      },
      {
        flag: '--enable-compress-response-body',
        help: '压缩 HTTP 响应体（省带宽，客户端需要支持 gzip）',
      },
    ],
  },
]

export const ALL_FLAGS: FlagSpec[] = FLAG_GROUPS.flatMap((g) => g.flags)

export const FLAG_BY_NAME = new Map(ALL_FLAGS.map((f) => [f.flag, f]))

/** 由「参数行」取出参数名：支持 `--flag value` 和 `--flag=value` 两种写法 */
export function lineFlag(line: string): string | null {
  const token = line.trim().split(/\s+/, 1)[0]
  if (!token.startsWith('--')) return null
  const eq = token.indexOf('=')
  return eq > 0 ? token.slice(0, eq) : token
}

/** 由「参数行」取出值文本（没有值就是空串） */
export function lineValue(line: string): string {
  const text = line.trim()
  const token = text.split(/\s+/, 1)[0]
  const eq = token.indexOf('=')
  if (eq > 0) return token.slice(eq + 1)
  return text.slice(token.length).trim()
}

/** 生成勾选后的参数行 */
export function buildLine(spec: FlagSpec): string {
  if (!spec.value) return spec.flag
  return spec.value.def ? `${spec.flag} ${spec.value.def}` : spec.flag
}

/** 服务与网络那三个字段自己会生成参数，写在这里会重复 */
export const MANAGED_FLAGS = [
  '--listen',
  '--port',
  '--enable-cors-header',
  '--cors-origin',
  // 这三个由「存储」页按共享/自定义目录生成：手写会和 yaml / 启动计划打架
  '--extra-model-paths-config',
  '--input-directory',
  '--output-directory',
]

/** 互斥组的显示名：界面上把同组参数圈成一块，标题写明「只能选一个」 */
export const EXCLUSIVE_LABELS: Record<string, string> = {
  vram: '显存模式',
  fp: '全局精度',
  fpunet: '扩散模型（UNET）精度',
  fpvae: 'VAE 精度',
  fpte: '文本编码器精度',
  cache: '缓存方式',
  attn: '注意力实现',
  upcast: 'attention 上采样',
  malloc: 'CUDA 内存分配',
  dynamic_vram: '动态显存',
  disk: '磁盘卸载',
  triton: 'Triton 后端',
  manager_ui: 'Manager 界面',
  autolaunch: '浏览器自动打开',
  assets: '资产系统',
}

export interface FlagBlock {
  /** 互斥组 id；没有就是「独立开关」块 */
  exclusive?: string
  label: string
  flags: FlagSpec[]
}

/**
 * 把一个分组拆成「互斥块 + 独立开关块」。
 *
 * argparse 的互斥组是硬约束（同组两个一起给会直接报错），所以界面上不该把它们
 * 和普通开关混在一长串勾选框里 —— 拆成各自一小块，标题写「只能选一个」。
 */
export function blocksOf(group: FlagGroup): FlagBlock[] {
  const clusters: FlagBlock[] = []
  const plain: FlagSpec[] = []
  for (const spec of group.flags) {
    if (!spec.exclusive) {
      plain.push(spec)
      continue
    }
    let block = clusters.find((b) => b.exclusive === spec.exclusive)
    if (!block) {
      block = { exclusive: spec.exclusive, label: EXCLUSIVE_LABELS[spec.exclusive] ?? '互斥组', flags: [] }
      clusters.push(block)
    }
    block.flags.push(spec)
  }
  const blocks = [...clusters]
  if (plain.length > 0) blocks.push({ label: clusters.length > 0 ? '其他开关' : '', flags: plain })
  return blocks
}