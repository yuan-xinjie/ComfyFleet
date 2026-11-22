"""ComfyFleet 的 Python 启动壳。

实例仍然从 ComfyUI/main.py 启动，但先经过这个文件：
1. 装好进度桥，后续 import tqdm 时拿到的是被替换的类；
2. 把 sys.path / sys.argv 恢复成 ComfyUI 直接启动时的形状；
3. 用 runpy 执行 main.py，保持 __main__ 语义。

这个壳只对 ComfyFleet 拉起的进程生效，不写 site-packages，也不改用户环境。
"""

from __future__ import annotations

import os
import runpy
import sys


def _run() -> None:
    if len(sys.argv) < 2:
        raise SystemExit("launch.py: missing main script")

    script = os.path.abspath(sys.argv[1])
    args = sys.argv[2:]
    root = os.path.dirname(script)
    bridge_dir = os.path.dirname(os.path.abspath(__file__))

    # ComfyUI 默认把 main.py 所在目录放在 sys.path[0]；wrapper 启动时这里是
    # launch.py 所在目录，所以要显式补回去，否则 main.py 里的同级 import 会失败。
    sys.path[:] = [root, bridge_dir] + [
        p for p in sys.path if p not in (root, bridge_dir)
    ]

    # main.py 里可能检查 sys.argv[0]，保持“脚本路径 + 原始参数”的形状。
    sys.argv = [script] + args

    try:
        from comfyfleet_progress import install

        install()
    except Exception:
        # 进度桥只是增强功能，任何失败都不能影响 ComfyUI 启动。
        pass

    runpy.run_path(script, run_name="__main__")


if __name__ == "__main__":
    _run()
