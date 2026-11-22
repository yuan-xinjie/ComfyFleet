"""把 ComfyUI 的结构化进度通过 stdout 发给 ComfyFleet。

借鉴绘世启动器的做法：不去解析终端动画，而是在 Python 侧把进度条拆成
左右两段，和当前值/最大值一起上报。Rust 收到 ``\\x1eCFP`` 开头的 JSON 行后，
会把它从日志里摘掉，作为独立进度事件发给界面。

覆盖两类进度：

* tqdm：模型加载、下载等；
* ComfyUI 原生 ``comfy_execution.progress``：KSampler / VAE 等节点。

这里的实现只依赖 ComfyUI 和 tqdm 自身，不改 ComfyUI 代码，也不写 site-packages。
"""

from __future__ import annotations

import json
import math
import os
import re
import sys
import threading
import time
from typing import Any

# 与 Rust 侧 PROGRESS_PREFIX 保持一致。用 ASCII 控制字符开头，正常日志几乎不会撞上。
_PREFIX = "\x1eCFP "
_ENABLED = os.environ.get("COMFYFLEET_PY_PROGRESS") == "1"

_WRITE_LOCK = threading.Lock()
_SPEED_RE = re.compile(
    r"([\d.]+)\s*(it/s|s/it|[KMGTPE]i?B/s|[KMGT]?B/s)", re.IGNORECASE
)
_BAR_RE = re.compile(r"\{bar(?::.+?)?\}", re.DOTALL)
_BYTE_SPEED_RE = re.compile(r"[KMGTPE]i?B/s|[KMGT]?B/s", re.IGNORECASE)
# 原生节点进度在跑时，模型加载/下载类 tqdm 仍然保留；只压掉重复的
# 执行阶段进度（例如 KSampler 同时走 tqdm 和原生进度）。
_NON_EXECUTION_RE = re.compile(
    r"download|fetch|下载|拉取",
    re.IGNORECASE,
)
_VALUE_INTERVAL = 0.05
_TEXT_INTERVAL = 0.2
# 和 tqdm 默认值保持一致。
_RATE_MIN_INTERVAL = 0.1
_RATE_SMOOTHING = 0.3


class _Ema:
    """tqdm 的指数滑动平均：alpha 越大越偏向新样本。"""

    def __init__(self, alpha: float) -> None:
        self.alpha = alpha
        self.last = 0.0
        self.calls = 0

    def __call__(self, value: float) -> float:
        beta = 1.0 - self.alpha
        self.last = self.alpha * value + beta * self.last
        self.calls += 1
        return self.last / (1.0 - beta ** self.calls) if self.calls else self.last


class _TqdmRate:
    """按 tqdm 的方式估算速度。

    tqdm 不是每次 update 都重算，而是在刷新点（默认至少间隔 100ms）取
    这一段的增量，再对增量做 EMA。这里保留同一套逻辑，只是把结果直接
    发给 ComfyFleet，不需要真的创建 tqdm 对象。
    """

    def __init__(self) -> None:
        self._ema_dn = _Ema(_RATE_SMOOTHING)
        self._ema_dt = _Ema(_RATE_SMOOTHING)
        self._last_value = 0.0
        self._last_time = 0.0
        self._speed: str | None = None

    def start(self, value: float = 0.0) -> None:
        self._last_value = float(value or 0.0)
        self._last_time = time.monotonic()
        self._speed = None

    def update(self, value: float) -> str | None:
        now = time.monotonic()
        elapsed = now - self._last_time
        delta = float(value or 0.0) - self._last_value
        # tqdm 在 mininterval 内不刷新，增量会累积到下一次刷新。
        if elapsed < _RATE_MIN_INTERVAL:
            return self._speed

        self._last_time = now
        self._last_value = float(value or 0.0)
        if delta <= 0:
            return self._speed

        ema_dn = self._ema_dn(delta)
        ema_dt = self._ema_dt(elapsed)
        if ema_dt <= 0:
            return self._speed
        rate = ema_dn / ema_dt
        if not math.isfinite(rate) or rate <= 0:
            return self._speed
        # tqdm 在低于 1 个/秒时会反过来显示秒/步。
        inverse = 1.0 / rate
        self._speed = f"{inverse:.2f} s/it" if inverse > 1 else f"{rate:.2f} it/s"
        return self._speed


def _emit(payload: dict[str, Any]) -> None:
    """写一行给父进程。

    用 ``sys.__stdout__`` 而不是 ``sys.stdout``：ComfyUI / 某些节点会重定向
    stdout 做自己的日志；进度事件必须沿原始管道走。写失败直接放弃，不能让
    主进程因为 UI 增强功能崩掉。
    """

    stream = sys.__stdout__
    if stream is None:
        return
    try:
        data = json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode(
            "utf-8"
        )
    except Exception:
        return

    with _WRITE_LOCK:
        try:
            buffer = getattr(stream, "buffer", None)
            if buffer is not None:
                buffer.write(_PREFIX.encode("ascii") + data + b"\n")
                buffer.flush()
            else:
                stream.write(_PREFIX + data.decode("utf-8") + "\n")
                stream.flush()
        except Exception:
            pass


def _strip_char(value: str, char: str) -> str:
    if value.startswith(char):
        value = value[len(char) :]
    if value.endswith(char):
        value = value[: -len(char)]
    return value


def _speed_from_right(right: str) -> str | None:
    match = _SPEED_RE.search(right)
    if not match:
        return None
    number = float(match.group(1))
    unit = match.group(2)
    if unit.lower() == "s/it":
        if number <= 0:
            return None
        return f"{1.0 / number:.2f} it/s"
    if unit.lower() == "it/s":
        if number <= 0:
            return None
        return f"{number:.2f} it/s"
    # 字节速度保留日志里的单位，例如 MB/s、MiB/s。
    return f"{match.group(1)} {unit}"


def _is_download_bar(name: str, right: str, speed: str | None) -> bool:
    """tqdm / rich 条是不是下载，而不是节点执行。"""

    text = f"{name} {right} {speed or ''}"
    return bool(_BYTE_SPEED_RE.search(text) or _NON_EXECUTION_RE.search(text))


class _Bridge:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._records: dict[str, dict[str, Any]] = {}
        self._last_value_emit: dict[str, float] = {}
        self._last_text_emit: dict[str, float] = {}
        self._sources: dict[str, str] = {}
        self._native_active = False
        self._native_until = 0.0

    def set_native_active(self, active: bool) -> None:
        now = time.monotonic()
        stale: list[str] = []
        with self._lock:
            changed = self._native_active != active
            self._native_active = active
            self._native_until = 0.0 if active else now + 0.5
            if changed:
                # 原生节点切换时清掉旧的 tqdm 执行条；下载条保留。
                for key, source in list(self._sources.items()):
                    if source == "native":
                        continue
                    record = self._records.get(key)
                    if record is None:
                        continue
                    if _is_download_bar(
                        str(record.get("name") or ""),
                        str(record.get("right") or ""),
                        record.get("speed"),
                    ):
                        continue
                    stale.append(key)
                for key in stale:
                    self._records.pop(key, None)
                    self._last_value_emit.pop(key, None)
                    self._last_text_emit.pop(key, None)
                    self._sources.pop(key, None)
        for key in stale:
            _emit({"op": "remove", "id": key})

    def clear_all(self) -> None:
        """清掉所有进度；用于任务成功、报错或被取消时收尾。"""

        with self._lock:
            keys = list(self._records)
            self._records.clear()
            self._last_value_emit.clear()
            self._last_text_emit.clear()
            self._sources.clear()
            self._native_active = False
            self._native_until = 0.0
        for key in keys:
            _emit({"op": "remove", "id": key})

    def update(
        self,
        key: str,
        *,
        name: str,
        indeterminate: bool,
        value: float,
        total: float,
        left: str,
        right: str,
        speed: str | None,
        force: bool = False,
        source: str = "tqdm",
    ) -> None:
        now = time.monotonic()
        current = float(value or 0)
        maximum = float(total or 0)
        record = {
            "op": "update",
            "id": key,
            "name": name,
            "indeterminate": bool(indeterminate),
            "value": current,
            "max": maximum,
            "left": left,
            "right": right,
            "speed": speed,
        }

        with self._lock:
            if source != "native" and (self._native_active or now < self._native_until):
                # 节点执行已经有原生进度，tqdm / rich 只保留下载；否则会出现
                # “正在准备”和“正在采样”两条进度互相抢更新。
                if not _is_download_bar(name, right, speed):
                    return

            previous = self._records.get(key)
            if previous is None:
                self._records[key] = record
                self._sources[key] = source
                self._last_value_emit[key] = now
                self._last_text_emit[key] = now
                emit = True
            else:
                stage_changed = (
                    previous["name"] != record["name"]
                    or previous["indeterminate"] != record["indeterminate"]
                    or previous["max"] != record["max"]
                )
                value_changed = previous["value"] != current
                text_changed = (
                    previous["left"] != left
                    or previous["right"] != right
                    or previous["speed"] != speed
                )
                finished = maximum > 0 and current >= maximum and value_changed

                emit = bool(force or stage_changed or finished)
                if not emit:
                    emit = bool(
                        value_changed
                        and now - self._last_value_emit.get(key, 0.0) >= _VALUE_INTERVAL
                    )
                if not emit:
                    emit = bool(
                        text_changed
                        and now - self._last_text_emit.get(key, 0.0) >= _TEXT_INTERVAL
                    )

                if emit:
                    self._records[key] = record
                    if stage_changed or value_changed:
                        self._last_value_emit[key] = now
                    if stage_changed or text_changed:
                        self._last_text_emit[key] = now

        if emit:
            _emit(record)

    def remove(self, key: str) -> None:
        with self._lock:
            self._records.pop(key, None)
            self._last_value_emit.pop(key, None)
            self._last_text_emit.pop(key, None)
            self._sources.pop(key, None)
        _emit({"op": "remove", "id": key})


class _ComfyNativeHandler:
    """挂到 ComfyUI 的进度注册表上，捕获 KSampler / VAE 等原生进度。"""

    name = "comfyfleet"
    enabled = True

    def __init__(self, bridge: _Bridge) -> None:
        self.bridge = bridge
        self.registry: Any = None
        self._keys: set[str] = set()
        self._rates: dict[str, _TqdmRate] = {}

    def set_registry(self, registry: Any) -> None:
        self.registry = registry

    def _sync_active(self) -> None:
        self.bridge.set_native_active(bool(self._keys))

    def _node_name(self, node_id: str) -> str:
        try:
            node = self.registry.dynprompt.get_node(node_id)
            class_type = node.get("class_type") if isinstance(node, dict) else None
            if class_type:
                return str(class_type)
        except Exception:
            pass
        return str(node_id or "")

    def start_handler(self, node_id: str, state: Any, prompt_id: str) -> None:
        try:
            name = self._node_name(node_id)
            # 每个节点都亮一条：前端能识别的显示阶段，不认识的显示
            # “正在执行 <节点名>”。
            key = f"comfy:{prompt_id}:{node_id}"
            self._keys.add(key)
            self._sync_active()
            rate = self._rates.get(key)
            if rate is None:
                rate = _TqdmRate()
                self._rates[key] = rate
            rate.start(0.0)
            self.bridge.update(
                key,
                name=name,
                indeterminate=True,
                value=0,
                total=0,
                left="",
                right="",
                speed=None,
                force=True,
                source="native",
            )
        except Exception:
            pass

    def update_handler(
        self,
        node_id: str,
        value: float,
        max_value: float,
        state: Any,
        prompt_id: str,
        image: Any = None,
    ) -> None:
        try:
            key = f"comfy:{prompt_id}:{node_id}"
            self._keys.add(key)
            self._sync_active()
            current = float(value or 0)
            total = float(max_value or 0)
            if not math.isfinite(current):
                current = 0.0
            if not math.isfinite(total):
                total = 0.0

            rate = self._rates.get(key)
            if rate is None:
                rate = _TqdmRate()
                self._rates[key] = rate
                rate.start(0.0)
            speed = rate.update(current)

            percent = (
                int(round(max(0.0, min(100.0, current / total * 100))))
                if total > 0
                else 0
            )
            self.bridge.update(
                key,
                name=self._node_name(node_id),
                indeterminate=total <= 0,
                value=current,
                total=total,
                left=f"{percent}%" if total > 0 else "",
                right=f"{current:g}/{total:g}" if total > 0 else "",
                speed=speed,
                source="native",
            )
        except Exception:
            pass

    def finish_handler(self, node_id: str, state: Any, prompt_id: str) -> None:
        key = f"comfy:{prompt_id}:{node_id}"
        self._keys.discard(key)
        self._rates.pop(key, None)
        self._sync_active()
        self.bridge.remove(key)

    def reset(self) -> None:
        for key in list(self._keys):
            self.bridge.remove(key)
        self._keys.clear()
        self._rates.clear()
        self._sync_active()

    def enable(self) -> None:
        self.enabled = True

    def disable(self) -> None:
        self.enabled = False


def _format_side(tqdm_obj: Any, bar_format: str, format_dict: dict[str, Any]) -> str:
    values = dict(format_dict)
    values["bar_format"] = bar_format
    text = tqdm_obj.format_meter(**values)
    return _strip_char(text, "|").strip()


def _patch_tqdm(bridge: _Bridge) -> None:
    try:
        import tqdm as tqdm_module
        import tqdm.std as tqdm_std
    except Exception:
        return

    base = getattr(tqdm_std, "tqdm", None)
    if base is None:
        return

    class ComfyFleetTqdm(base):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            kwargs = kwargs.copy()
            kwargs["gui"] = True
            kwargs["disable"] = bool(kwargs.get("disable", False))
            # super().__init__ 可能立刻调用 display()，所以先放好标记。
            self._cf_progress_hash = 0
            super().__init__(*args, **kwargs)
            if not getattr(self, "disable", True):
                try:
                    self.display(check_delay=False)
                except Exception:
                    pass

        def clear(self, *args: Any, **kwargs: Any) -> None:
            # 不让 tqdm 往终端回退；清屏交给 ComfyFleet 的日志流处理。
            return

        def display(self, *args: Any, **kwargs: Any) -> None:
            if getattr(self, "disable", True):
                return
            try:
                key = f"py:{id(self)}"
                total = self.total
                indeterminate = total is None
                desc = self.desc or ""
                if desc.endswith(": "):
                    desc = desc[:-2]

                bar_format = self.bar_format or "{l_bar}{bar}{r_bar}"
                if not isinstance(bar_format, str):
                    bar_format = "{l_bar}{bar}{r_bar}"
                bar_format = bar_format.replace("{desc}: ", "")

                format_dict = dict(self.format_dict)
                format_dict["prefix"] = ""

                # tqdm 默认格式是 {l_bar}{bar}{r_bar}；自定义格式可能把 bar
                # 放在别的位置，甚至没有 {bar}，所以保留和绘世一致的兜底拆分。
                parts = _BAR_RE.split(bar_format, maxsplit=1)
                pipe_parts = bar_format.split("|")
                if len(parts) == 2:
                    left_format, right_format = parts
                elif "{r_bar}" in bar_format:
                    left_format = bar_format.replace("{r_bar}", "")
                    right_format = "{r_bar}"
                elif len(pipe_parts) >= 2:
                    left_format = pipe_parts[0]
                    right_format = "|".join(pipe_parts[1:])
                else:
                    left_format = bar_format
                    right_format = ""

                left = _format_side(self, left_format.strip(), format_dict)
                right = _format_side(self, right_format.strip(), format_dict)
                if (
                    right.count("[") == 1
                    and right.count("]") == 1
                    and right.startswith("[")
                    and right.endswith("]")
                ):
                    right = _strip_char(right, "[")
                    right = _strip_char(right, "]")

                current = float(self.n or 0)
                total_value = float(total) if total is not None else 0.0
                # tqdm 允许 total=inf；JSON 不能写 Infinity，否则 Rust 解析失败。
                if not math.isfinite(current):
                    current = 0.0
                if not math.isfinite(total_value):
                    indeterminate = True
                    total_value = 0.0
                speed = _speed_from_right(right)

                new_hash = hash((indeterminate, total_value, desc))
                bridge.update(
                    key,
                    name=desc,
                    indeterminate=indeterminate,
                    value=current,
                    total=total_value,
                    left=left,
                    right=right,
                    speed=speed,
                    force=new_hash != self._cf_progress_hash,
                )
                self._cf_progress_hash = new_hash
            except Exception:
                pass

        def close(self) -> None:
            if getattr(self, "disable", True):
                return
            self.disable = True
            try:
                bridge.remove(f"py:{id(self)}")
            except Exception:
                pass
            try:
                with self.get_lock():
                    self._instances.remove(self)
            except Exception:
                pass

    def trange(*args: Any, **kwargs: Any):
        from tqdm.utils import _range

        return ComfyFleetTqdm(_range(*args), **kwargs)

    tqdm_module.tqdm = ComfyFleetTqdm
    tqdm_module.trange = trange
    tqdm_std.tqdm = ComfyFleetTqdm

    try:
        import tqdm.auto as tqdm_auto

        tqdm_auto.tqdm = ComfyFleetTqdm
        tqdm_auto.trange = trange
    except Exception:
        pass


_RICH_MODULES = ("rich.progress", "pip._vendor.rich.progress")
_RICH_FINDER: Any = None


class _RichLoader:
    """先让原 loader 执行，再把 rich.progress.Progress 换成桥接版。"""

    def __init__(self, loader: Any, fullname: str) -> None:
        self.loader = loader
        self.fullname = fullname

    def create_module(self, spec: Any) -> Any:
        factory = getattr(self.loader, "create_module", None)
        if factory is None:
            return None
        return factory(spec)

    def exec_module(self, module: Any) -> None:
        self.loader.exec_module(module)
        _patch_rich_module(module)


class _RichFinder:
    """在 rich / pip vendored rich 被 import 时挂钩。

    tqdm 是启动时就 import 的，rich 往往是后面某个节点或 pip 才 import；
    只靠启动时 patch 会漏掉。
    """

    def find_spec(self, fullname: str, path: Any = None, target: Any = None) -> Any:
        if fullname not in _RICH_MODULES:
            return None
        for finder in sys.meta_path:
            if finder is self:
                continue
            find = getattr(finder, "find_spec", None)
            if find is None:
                continue
            try:
                spec = find(fullname, path, target)
            except Exception:
                continue
            if spec is not None and getattr(spec, "loader", None) is not None:
                spec.loader = _RichLoader(spec.loader, fullname)
                return spec
        return None


def _patch_rich_module(module: Any) -> None:
    bridge = _BRIDGE
    base = getattr(module, "Progress", None)
    if bridge is None or base is None or getattr(base, "_comfyfleet_patched", False):
        return

    module_name = getattr(module, "__name__", "")
    if not module_name.endswith("progress"):
        return
    root = module_name.rsplit(".", 1)[0]

    class ComfyFleetRichProgress(base):
        def __init__(self, *args: Any, **kwargs: Any) -> None:
            kwargs = kwargs.copy()
            kwargs["auto_refresh"] = False
            super().__init__(*args, **kwargs)
            self._cf_visible: dict[int, bool] = {}

        def __del__(self, *args: Any, **kwargs: Any) -> None:
            try:
                self.stop()
            except Exception:
                pass

        def start(self, *args: Any, **kwargs: Any) -> None:
            # 不启动 rich 自己的刷新线程；refresh() 由 update() 驱动。
            return

        def stop(self, *args: Any, **kwargs: Any) -> None:
            try:
                with self._lock:
                    for task in list(self.tasks):
                        bridge.remove(f"rich:{id(task)}")
                        self._cf_visible.pop(id(task), None)
            except Exception:
                pass
            try:
                super().stop(*args, **kwargs)
            except Exception:
                pass

        def remove_task(self, task_id: Any, *args: Any, **kwargs: Any) -> None:
            try:
                with self._lock:
                    task = self._tasks.get(task_id)
                    if task is not None:
                        bridge.remove(f"rich:{id(task)}")
                        self._cf_visible.pop(id(task), None)
            except Exception:
                pass
            super().remove_task(task_id, *args, **kwargs)

        def update(self, *args: Any, **kwargs: Any) -> Any:
            kwargs["refresh"] = True
            return super().update(*args, **kwargs)

        def refresh(self, *args: Any, **kwargs: Any) -> None:
            try:
                bar_column = getattr(module, "BarColumn", None)
                with self._lock:
                    active: set[int] = set()
                    for task in list(self.tasks):
                        clsid = id(task)
                        active.add(clsid)
                        key = f"rich:{clsid}"
                        visible = bool(getattr(task, "visible", True))
                        if self._cf_visible.get(clsid) != visible:
                            self._cf_visible[clsid] = visible
                            if not visible:
                                bridge.remove(key)
                                continue
                        if not visible:
                            continue

                        total = getattr(task, "total", None)
                        indeterminate = total is None
                        maximum = float(total or 0)
                        value = float(getattr(task, "completed", 0) or 0)
                        left = ""
                        right = ""
                        left_side = True
                        for column in list(self.columns):
                            if isinstance(column, str):
                                if left_side:
                                    left += column + " "
                                else:
                                    right += " " + column
                            elif bar_column is not None and isinstance(column, bar_column):
                                left_side = False
                            else:
                                rendered = column.render(task)
                                plain = getattr(rendered, "plain", rendered)
                                if not isinstance(plain, str):
                                    plain = str(plain)
                                if left_side:
                                    left += plain + " "
                                else:
                                    right += " " + plain
                        left = left.strip()
                        right = right.strip()
                        name = str(getattr(task, "description", "") or "")
                        bridge.update(
                            key,
                            name=name,
                            indeterminate=indeterminate,
                            value=value,
                            total=maximum,
                            left=left,
                            right=right,
                            speed=_speed_from_right(right),
                            source="rich",
                        )

                    for clsid in list(self._cf_visible):
                        if clsid not in active:
                            bridge.remove(f"rich:{clsid}")
                            self._cf_visible.pop(clsid, None)
            except Exception:
                pass

    ComfyFleetRichProgress._comfyfleet_patched = True  # type: ignore[attr-defined]
    module.Progress = ComfyFleetRichProgress


def _install_rich_progress() -> None:
    global _RICH_FINDER
    if _RICH_FINDER is not None:
        return
    _RICH_FINDER = _RichFinder()
    sys.meta_path.insert(0, _RICH_FINDER)
    for name in _RICH_MODULES:
        module = sys.modules.get(name)
        if module is not None:
            try:
                _patch_rich_module(module)
            except Exception:
                pass


def _patch_comfy_progress(module: Any, bridge: _Bridge) -> None:
    """给每个新 Prompt 的 ProgressRegistry 自动挂一个桥 handler。"""

    registry_cls = getattr(module, "ProgressRegistry", None)
    if registry_cls is None or getattr(registry_cls, "_comfyfleet_patched", False):
        return

    original_init = registry_cls.__init__

    def patched_init(self: Any, *args: Any, **kwargs: Any) -> None:
        original_init(self, *args, **kwargs)
        try:
            handler = _ComfyNativeHandler(bridge)
            handler.set_registry(self)
            self.register_handler(handler)
        except Exception:
            pass

    registry_cls.__init__ = patched_init
    registry_cls._comfyfleet_patched = True

    # 如果 patch 之前已经有 registry，也补挂一次。
    existing = getattr(module, "global_progress_registry", None)
    if existing is not None:
        try:
            handler = _ComfyNativeHandler(bridge)
            handler.set_registry(existing)
            existing.register_handler(handler)
        except Exception:
            pass


def _install_comfy_progress(bridge: _Bridge) -> None:
    """等 ComfyUI 导入进度模块后再打补丁，不提前 import 它。"""

    def apply() -> bool:
        module = sys.modules.get("comfy_execution.progress")
        if module is None:
            return False
        try:
            _patch_comfy_progress(module, bridge)
            return True
        except Exception:
            return False

    if apply():
        return

    def wait_and_patch() -> None:
        deadline = time.monotonic() + 120.0
        while time.monotonic() < deadline:
            if apply():
                return
            time.sleep(0.05)

    threading.Thread(
        target=wait_and_patch,
        name="comfyfleet-native-progress",
        daemon=True,
    ).start()


_BRIDGE: _Bridge | None = None


def _install_execution_lifecycle(bridge: _Bridge) -> None:
    """任务成功、报错、取消时清空进度，不清空的话最后一条节点/tqdm 条会一直留着。"""

    clear_events = {
        "execution_start",
        "execution_success",
        "execution_error",
        "execution_interrupted",
    }

    def apply() -> bool:
        module = sys.modules.get("execution")
        if module is None:
            return False
        executor = getattr(module, "PromptExecutor", None)
        if executor is None or getattr(executor, "_comfyfleet_lifecycle", False):
            return False

        original = executor.add_message

        def patched(self: Any, event: str, data: dict[str, Any], broadcast: bool) -> None:
            if event in clear_events:
                try:
                    bridge.clear_all()
                except Exception:
                    pass
            return original(self, event, data, broadcast)

        executor.add_message = patched
        executor._comfyfleet_lifecycle = True
        return True

    if apply():
        return

    def wait_and_patch() -> None:
        deadline = time.monotonic() + 120.0
        while time.monotonic() < deadline:
            if apply():
                return
            time.sleep(0.05)

    threading.Thread(
        target=wait_and_patch,
        name="comfyfleet-execution-lifecycle",
        daemon=True,
    ).start()


def install() -> _Bridge | None:
    """安装进度桥；重复调用是安全的。"""

    global _BRIDGE
    if not _ENABLED:
        return None
    if _BRIDGE is not None:
        return _BRIDGE
    _BRIDGE = _Bridge()
    _patch_tqdm(_BRIDGE)
    _install_rich_progress()
    _install_comfy_progress(_BRIDGE)
    _install_execution_lifecycle(_BRIDGE)
    return _BRIDGE
