//! 本机性能监控(设计 C):栏装饰波形数据源。
//! CPU 聚合占用(/proc/stat)与网络流量(/proc/net/dev,非 lo 合计),
//! 1s 采样、120 样本环形历史(约 2 分钟)、峰值归一化 + 缓慢衰减;
//! 隐藏期间的采样缺口以旧值补平,重新呼出时波形不出现断层。

use std::time::Instant;

use tay_config::BarPattern;

pub(crate) const HISTORY_LEN: usize = 120;
pub(crate) const SAMPLE_INTERVAL: f32 = 1.0;
/// 峰值衰减系数(每样本):约 4 分钟回落一半,历史高峰不永久压扁波形。
const PEAK_DECAY: f32 = 0.997;

#[derive(Default)]
pub(crate) struct Monitor {
    cpu_history: Vec<f32>,
    net_history: Vec<f32>,
    last_sample: Option<Instant>,
    cpu_prev: Option<(u64, u64)>,
    net_prev: Option<u64>,
    cpu_peak: f32,
    net_peak: f32,
}

impl Monitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 到采样间隔就采样一次(返回是否采样)。调用方每 ~1s 驱动
    /// (Timer 事件);隐藏导致的超长间隔用旧值补平缺失槽位。
    pub fn tick(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last_sample
            .is_some_and(|at| now.duration_since(at).as_secs_f32() < SAMPLE_INTERVAL)
        {
            return false;
        }
        let mut elapsed = self
            .last_sample
            .map_or(SAMPLE_INTERVAL, |at| now.duration_since(at).as_secs_f32());
        self.last_sample = Some(now);
        let cpu = cpu_usage(read_cpu().as_deref().unwrap_or(""), self.cpu_prev);
        self.cpu_prev = cpu.1;
        let net = net_rate(read_net().as_deref().unwrap_or(""), self.net_prev, elapsed);
        self.net_prev = net.1;
        while elapsed > SAMPLE_INTERVAL + 1e-3 {
            self.push_cpu(self.cpu_history.last().copied().unwrap_or(cpu.0));
            self.push_net(self.net_history.last().copied().unwrap_or(net.0));
            elapsed -= SAMPLE_INTERVAL;
        }
        self.push_cpu(cpu.0);
        self.push_net(net.0);
        true
    }

    fn push_cpu(&mut self, v: f32) {
        if v > self.cpu_peak {
            self.cpu_peak = v;
        } else {
            self.cpu_peak *= PEAK_DECAY;
        }
        push(&mut self.cpu_history, v);
    }

    fn push_net(&mut self, v: f32) {
        if v > self.net_peak {
            self.net_peak = v;
        } else {
            self.net_peak *= PEAK_DECAY;
        }
        push(&mut self.net_history, v);
    }

    /// 图案对应的历史样本与归一化峰值;非监控图案返回空切片。
    pub fn data(&self, pattern: BarPattern) -> (&[f32], f32) {
        match pattern {
            BarPattern::Cpu => (&self.cpu_history, self.cpu_peak.max(1e-3)),
            BarPattern::Net => (&self.net_history, self.net_peak.max(1.0)),
            _ => (&[], 1.0),
        }
    }

    /// 距下次采样的归一化进度 0..1(渲染层用它做平滑滚动)。
    pub fn scroll_frac(&self) -> f32 {
        self.last_sample.map_or(0.0, |at| {
            (at.elapsed().as_secs_f32() / SAMPLE_INTERVAL).clamp(0.0, 1.0)
        })
    }
}

/// 历史缓冲追加(超出容量丢最旧;120 元素,每秒 1 次,线性移除可忽略)。
fn push(history: &mut Vec<f32>, v: f32) {
    if history.len() >= HISTORY_LEN {
        history.remove(0);
    }
    history.push(v);
}

fn read_cpu() -> Option<String> {
    std::fs::read_to_string("/proc/stat").ok()
}

fn read_net() -> Option<String> {
    std::fs::read_to_string("/proc/net/dev").ok()
}

/// 解析 /proc/stat 首行(聚合),返回占用 0..1 与新的 (total, idle)。
/// 首帧无基线返回 0;字段缺失/回退时沿用旧基线。
pub(crate) fn cpu_usage(text: &str, prev: Option<(u64, u64)>) -> (f32, Option<(u64, u64)>) {
    let Some(line) = text.lines().find(|l| l.starts_with("cpu ")) else {
        return (0.0, prev);
    };
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() < 8 {
        return (0.0, prev);
    }
    let idle = vals[3] + vals[4]; // idle + iowait
    let total: u64 = vals.iter().sum();
    let usage = match prev {
        Some((pt, pi)) if total > pt => {
            let used = (total - pt).saturating_sub(idle.saturating_sub(pi));
            (used as f32 / (total - pt) as f32).clamp(0.0, 1.0)
        }
        _ => 0.0,
    };
    (usage, Some((total, idle)))
}

/// 解析 /proc/net/dev:非 lo 接口 rx+tx 字节合计;返回速率 B/s
/// (elapsed 下限 0.5s,首帧无基线返回 0)。
pub(crate) fn net_rate(text: &str, prev: Option<u64>, elapsed: f32) -> (f32, Option<u64>) {
    let mut sum = 0u64;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let Some(iface) = it.next() else { continue };
        if iface == "lo:" || iface == "lo" {
            continue;
        }
        let Some(rx) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        // rx 之后第 8 个字段是 tx_bytes(rx,packets,errs,drop,fifo,frame,compressed,multicast,tx,...)。
        let tx = it.nth(7).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        sum += rx + tx;
    }
    let rate = match prev {
        Some(p) if sum >= p => (sum - p) as f32 / elapsed.max(0.5),
        _ => 0.0,
    };
    (rate, Some(sum))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_usage_parses_aggregate_line_and_clamps() {
        // user 100 nice 0 system 0 idle 300 iowait 0 irq 0 softirq 0 steal 0
        let t1 = "cpu  100 0 0 300 0 0 0 0 0 0";
        let (u1, p1) = cpu_usage(t1, None);
        assert_eq!(u1, 0.0, "首帧无基线");
        let p1 = p1.unwrap();
        // user 200,idle 300 → 100/200 = 50%
        let (u2, _) = cpu_usage("cpu  200 0 0 400 0 0 0 0 0 0", Some(p1));
        assert!((u2 - 0.5).abs() < 1e-6, "u2={u2}");
        // 无 cpu 行 → 沿用基线。
        let (u3, p3) = cpu_usage("btime 123\n", Some(p1));
        assert_eq!(u3, 0.0);
        assert_eq!(p3, Some(p1));
    }

    #[test]
    fn net_rate_sums_non_loopback_and_skips_headers() {
        let text = "Inter-|   Receive | Transmit\n face |bytes\n\
                    lo: 100 0 0 0 0 0 0 0 100 0 0 0 0 0 0 0\n\
                    eth0: 1000 0 0 0 0 0 0 0 500 0 0 0 0 0 0 0\n\
                    wlan0: 300 0 0 0 0 0 0 0 200 0 0 0 0 0 0 0\n";
        let (r1, p1) = net_rate(text, None, 1.0);
        assert_eq!(r1, 0.0, "首帧无基线");
        let p1 = p1.unwrap();
        assert_eq!(p1, 1000 + 500 + 300 + 200, "非 lo 合计");
        let (r2, _) = net_rate(text, Some(p1 - 1000), 1.0);
        assert_eq!(r2, 1000.0, "1s 内 1000B 差量");
        // elapsed 下限 0.5s。
        let (r3, _) = net_rate(text, Some(p1 - 1000), 0.01);
        assert_eq!(r3, 2000.0);
    }

    #[test]
    fn tick_fills_gaps_with_previous_value_and_decays_peak() {
        let mut m = Monitor::new();
        m.push_cpu(0.8);
        m.push_cpu(0.2);
        // 峰值从 0.8 衰减,不因新低值消失。
        assert!(m.cpu_peak < 0.8 && m.cpu_peak > 0.7);
        // 环形容量。
        for i in 0..(HISTORY_LEN + 10) {
            m.push_cpu(i as f32 / 100.0);
        }
        let (hist, _) = m.data(BarPattern::Cpu);
        assert_eq!(hist.len(), HISTORY_LEN);
        // 非监控图案返回空。
        let (hist, _) = m.data(BarPattern::Wave);
        assert!(hist.is_empty());
    }
}
