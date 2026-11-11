//! 本机物理网卡吞吐采样：给安装/节点/切版本显示网络速度，同步任务显示局域网速度。
//!
//! 不再从目录增长推算磁盘写入速度。

use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetSpeed {
    pub rx_bps: f64,
    pub tx_bps: f64,
}

struct Sampler {
    prev: Option<(u64, u64, Instant)>,
    last: NetSpeed,
}

static SAMPLER: OnceLock<Mutex<Sampler>> = OnceLock::new();

pub fn sample() -> NetSpeed {
    let mut sampler = SAMPLER
        .get_or_init(|| {
            Mutex::new(Sampler {
                prev: None,
                last: NetSpeed::default(),
            })
        })
        .lock();
    let Some((rx, tx)) = counters() else {
        return sampler.last;
    };
    let now = Instant::now();
    if let Some((prev_rx, prev_tx, prev_at)) = sampler.prev {
        let seconds = now.duration_since(prev_at).as_secs_f64();
        // 太短的间隔（例如两个窗口同时轮询）不重算，避免除以抖动。
        if seconds >= 0.25 {
            let rx_delta = rx.saturating_sub(prev_rx) as f64;
            let tx_delta = tx.saturating_sub(prev_tx) as f64;
            sampler.last = NetSpeed {
                rx_bps: rx_delta / seconds,
                tx_bps: tx_delta / seconds,
            };
            sampler.prev = Some((rx, tx, now));
        }
    } else {
        sampler.prev = Some((rx, tx, now));
    }
    sampler.last
}

#[cfg(windows)]
fn counters() -> Option<(u64, u64)> {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;

    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut table) != NO_ERROR || table.is_null() {
            return None;
        }
        let count = (*table).NumEntries as usize;
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        let mut rx = 0u64;
        let mut tx = 0u64;
        for row in rows {
            // MIB_IF_TYPE_LOOPBACK：回环流量不是真的网络传输。
            if row.Type == 24 || row.OperStatus != IfOperStatusUp {
                continue;
            }
            // WAN Miniport (IP / IPv6 / Network Monitor) 这类过滤/隧道接口会把
            // 同一份流量再统计一遍；只取 HardwareInterface，避免成倍放大。
            // bit 0 是 HardwareInterface。
            if row.InterfaceAndOperStatusFlags._bitfield & 0x01 == 0 {
                continue;
            }
            rx = rx.saturating_add(row.InOctets);
            tx = tx.saturating_add(row.OutOctets);
        }
        FreeMibTable(table as *const _);
        Some((rx, tx))
    }
}

#[cfg(not(windows))]
fn counters() -> Option<(u64, u64)> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn windows_counters_are_readable() {
        assert!(super::counters().is_some(), "应能读取网卡累计字节");
    }
}
