use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Pressure levels — graduated like a rubber band under tension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PressureLevel {
    None = 0,
    Mild = 1,
    Moderate = 2,
    Heavy = 3,
    Critical = 4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMetrics {
    pub vmmem_rss: u64,
    pub host_memory_total: u64,
    pub host_memory_avail: u64,
    pub wslconfig_memory_limit: Option<u64>,

    pub guest_resident: u64,
    pub guest_file_cache: u64,
    pub guest_cpu_percent: f32,
    pub guest_io_rate: f32,

    pub gap: u64,
}

/// All thresholds are **ratios**, not absolute byte counts, so the
/// algorithm scales naturally across 16 GB laptops and 128 GB servers.
///
/// The one absolute value is `baseline_gap` — the inherent overhead of
/// WSL2/Hyper-V that should never be reclaimed (idle Debian ~1.2 GB in
/// guest ↔ 3-4 GB vmmem ⇒ ~2 GB baseline).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ReclamationConfig {
    // ----- baseline -----
    /// Fixed gap that is expected WSL2/Hyper-V overhead and should be
    /// subtracted before scoring. Default 2 GB.
    pub baseline_gap: u64,

    // ----- gap thresholds (fraction of host_memory_total) -----
    pub gap_ratio_mild: f32,     // 0.01  (32 GB → 320 MB)
    pub gap_ratio_moderate: f32, // 0.03  (32 GB → 960 MB)
    pub gap_ratio_heavy: f32,    // 0.06  (32 GB → 1.92 GB)
    pub gap_ratio_critical: f32, // 0.12  (32 GB → 3.84 GB)

    // ----- host memory utilisation thresholds -----
    pub host_memory_warning: f32,  // 0.70
    pub host_memory_pressure: f32, // 0.80

    // ----- guest idle detection -----
    pub guest_cpu_idle: f32, // 0.05  (5 %)
    pub guest_io_idle: f32,  // 10.0  MB/s

    // ----- reclaim sizing (all fractions of host_memory_total) -----
    pub reclaim_ratio_moderate: f32, // 0.20 of effective gap
    pub reclaim_ratio_heavy: f32,    // 0.50 of effective gap
    pub reclaim_floor_ratio: f32,    // 0.001  min bytes to reclaim
    pub reclaim_cap_moderate: f32,   // 0.008  max for Moderate
    pub reclaim_cap_heavy: f32,      // 0.016  max for Heavy

    // ----- timing -----
    pub sustained_windows: usize,
    pub cooldown_moderate: Duration,
    pub cooldown_heavy: Duration,
    pub cooldown_critical: Duration,
}

impl Default for ReclamationConfig {
    fn default() -> Self {
        Self {
            baseline_gap: 2 * GB,

            gap_ratio_mild: 0.01,
            gap_ratio_moderate: 0.03,
            gap_ratio_heavy: 0.06,
            gap_ratio_critical: 0.12,

            host_memory_warning: 0.70,
            host_memory_pressure: 0.80,

            guest_cpu_idle: 0.05,
            guest_io_idle: 10.0,

            reclaim_ratio_moderate: 0.20,
            reclaim_ratio_heavy: 0.50,
            reclaim_floor_ratio: 0.001,
            reclaim_cap_moderate: 0.008,
            reclaim_cap_heavy: 0.016,

            sustained_windows: 3,
            cooldown_moderate: Duration::from_secs(10),
            cooldown_heavy: Duration::from_secs(30),
            cooldown_critical: Duration::from_secs(600),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReclamationAction {
    NoAction,
    GradualReclaim { bytes: u64 },
    Compact,
    DropCaches { level: u8 },
}

// =========================================================================
// Decision engine
// =========================================================================

pub struct ElasticReclaimer {
    config: ReclamationConfig,
    metrics_history: VecDeque<SystemMetrics>,
    last_moderate_time: Option<Instant>,
    last_heavy_time: Option<Instant>,
    last_critical_time: Option<Instant>,
}

impl ElasticReclaimer {
    pub fn new(config: ReclamationConfig) -> Self {
        Self {
            config,
            metrics_history: VecDeque::with_capacity(10),
            last_moderate_time: None,
            last_heavy_time: None,
            last_critical_time: None,
        }
    }

    pub fn push_metrics(&mut self, metrics: SystemMetrics) {
        self.metrics_history.push_back(metrics);
        if self.metrics_history.len() > 10 {
            self.metrics_history.pop_front();
        }
    }

    /// Return the baseline-adjusted gap as a fraction of host memory.
    fn effective_gap_ratio(m: &SystemMetrics, baseline: u64) -> f64 {
        let eff = m.gap.saturating_sub(baseline);
        if m.host_memory_total > 0 {
            eff as f64 / m.host_memory_total as f64
        } else {
            0.0
        }
    }

    // ------------------------------------------------------------------
    // Five-dimension weighted scoring (ratio-based, no hard-coded bytes).
    //
    //   dim          weight   max    weighted_max
    //   gap_ratio      30%     4       1.20
    //   host_mem       25%     4       1.00
    //   wslconfig      15%     4       0.60
    //   cache          20%     4       0.80
    //   sustained      10%     3       0.30
    //                              ----------
    //                    theoretical max = 3.90
    //
    // Critical threshold = 3.5  →  reachable under extreme conditions.
    // ------------------------------------------------------------------

    pub fn calculate_pressure_level(&self) -> PressureLevel {
        if self.metrics_history.is_empty() {
            return PressureLevel::None;
        }
        let m = self.metrics_history.back().unwrap();
        let score = self.score_metrics(m);
        score_to_level(score)
    }

    fn score_metrics(&self, m: &SystemMetrics) -> f32 {
        let cfg = &self.config;
        let mut score = 0.0_f64;

        // Dim 1 — effective gap as % of host_total (30 %, max 4)
        let gr = Self::effective_gap_ratio(m, cfg.baseline_gap);
        let gap_score = if gr >= cfg.gap_ratio_critical as f64 {
            4.0
        } else if gr >= cfg.gap_ratio_heavy as f64 {
            3.0
        } else if gr >= cfg.gap_ratio_moderate as f64 {
            2.0
        } else if gr >= cfg.gap_ratio_mild as f64 {
            1.0
        } else {
            0.0
        };
        score += gap_score * 0.30;

        // Dim 2 — host memory utilisation (25 %, max 4)
        let host_used = if m.host_memory_total > 0 {
            1.0 - (m.host_memory_avail as f64 / m.host_memory_total as f64)
        } else {
            0.0
        };
        let host_score = if host_used > 0.95 {
            4.0
        } else if host_used > 0.90 {
            3.0
        } else if host_used > cfg.host_memory_pressure as f64 {
            2.0
        } else if host_used > cfg.host_memory_warning as f64 {
            1.0
        } else {
            0.0
        };
        score += host_score * 0.25;

        // Dim 3 — vmmem / wslconfig_limit (15 %, max 4)
        let wsl_score = m
            .wslconfig_memory_limit
            .filter(|&l| l > 0)
            .map(|limit| {
                let r = m.vmmem_rss as f64 / limit as f64;
                if r > 0.98 {
                    4.0
                } else if r > 0.95 {
                    3.0
                } else if r > 0.85 {
                    2.0
                } else if r > 0.70 {
                    1.0
                } else {
                    0.0
                }
            })
            .unwrap_or(0.0);
        score += wsl_score * 0.15;

        // Dim 4 — guest file-cache / resident (20 %, max 4)
        let cache_ratio = if m.guest_resident > 0 {
            m.guest_file_cache as f64 / m.guest_resident as f64
        } else {
            0.0
        };
        let cache_score = if cache_ratio > 0.90 {
            4.0
        } else if cache_ratio > 0.80 {
            3.0
        } else if cache_ratio > 0.60 {
            2.0
        } else if cache_ratio > 0.40 {
            1.0
        } else {
            0.0
        };
        score += cache_score * 0.20;

        // Dim 5 — sustained multi-dim pressure (10 %, max 3)
        let sustained = self.check_sustained_pressure();
        score += sustained as f64 * 0.10;

        score as f32
    }

    /// 0-3: how many of {gap, host_mem, cache} have been above threshold
    /// for the last `sustained_windows` consecutive samples.
    fn check_sustained_pressure(&self) -> u8 {
        let n = self.config.sustained_windows;
        if self.metrics_history.len() < n {
            return 0;
        }
        let recent: Vec<&SystemMetrics> = self.metrics_history.iter().rev().take(n).collect();
        let cfg = &self.config;

        let mut level = 0u8;

        if recent.iter().all(|m| {
            Self::effective_gap_ratio(m, cfg.baseline_gap) >= cfg.gap_ratio_moderate as f64
        }) {
            level += 1;
        }

        if recent.iter().all(|m| {
            m.host_memory_total > 0
                && (1.0 - m.host_memory_avail as f64 / m.host_memory_total as f64)
                    > cfg.host_memory_pressure as f64
        }) {
            level += 1;
        }

        if recent.iter().all(|m| {
            m.guest_resident > 0 && m.guest_file_cache as f64 / m.guest_resident as f64 > 0.60
        }) {
            level += 1;
        }

        level
    }

    pub fn is_system_idle(&self) -> bool {
        self.metrics_history.back().is_some_and(|m| {
            m.guest_cpu_percent < self.config.guest_cpu_idle
                && m.guest_io_rate < self.config.guest_io_idle
        })
    }

    // ------------------------------------------------------------------
    // Decision tree
    // ------------------------------------------------------------------

    pub fn decide_action(&mut self) -> ReclamationAction {
        let now = Instant::now();
        let pressure = self.calculate_pressure_level();

        match pressure {
            PressureLevel::None | PressureLevel::Mild => ReclamationAction::NoAction,

            PressureLevel::Moderate => {
                if !Self::cooldown_elapsed(
                    self.last_moderate_time,
                    self.config.cooldown_moderate,
                    now,
                ) {
                    return ReclamationAction::NoAction;
                }
                self.last_moderate_time = Some(now);
                self.reclaim_action(
                    self.config.reclaim_ratio_moderate,
                    self.config.reclaim_floor_ratio,
                    self.config.reclaim_cap_moderate,
                )
            }

            PressureLevel::Heavy => {
                if !Self::cooldown_elapsed(self.last_heavy_time, self.config.cooldown_heavy, now) {
                    return ReclamationAction::NoAction;
                }
                self.last_heavy_time = Some(now);

                if self.check_sustained_pressure() >= 2 {
                    ReclamationAction::Compact
                } else {
                    self.reclaim_action(
                        self.config.reclaim_ratio_heavy,
                        self.config.reclaim_floor_ratio,
                        self.config.reclaim_cap_heavy,
                    )
                }
            }

            PressureLevel::Critical => {
                if !Self::cooldown_elapsed(
                    self.last_critical_time,
                    self.config.cooldown_critical,
                    now,
                ) {
                    return self.fallback_heavy(now);
                }
                if self.is_system_idle() {
                    self.last_critical_time = Some(now);
                    ReclamationAction::DropCaches { level: 3 }
                } else {
                    // System busy under Critical pressure: use Heavy-tier
                    // actions (Compact / GradualReclaim) until idle.
                    // DropCaches intentionally skipped when system is busy.
                    self.fallback_heavy(now)
                }
            }
        }
    }

    fn fallback_heavy(&mut self, now: Instant) -> ReclamationAction {
        if !Self::cooldown_elapsed(self.last_heavy_time, self.config.cooldown_heavy, now) {
            return ReclamationAction::NoAction;
        }
        self.last_heavy_time = Some(now);
        self.reclaim_action(
            self.config.reclaim_ratio_heavy,
            self.config.reclaim_floor_ratio,
            self.config.reclaim_cap_heavy,
        )
    }

    /// Build a `GradualReclaim` sized as a fraction of the effective gap,
    /// clamped to `[floor_ratio, cap_ratio]` of `host_memory_total`.
    fn reclaim_action(
        &self,
        gap_fraction: f32,
        floor_ratio: f32,
        cap_ratio: f32,
    ) -> ReclamationAction {
        if let Some(m) = self.metrics_history.back() {
            let eff_gap = m.gap.saturating_sub(self.config.baseline_gap);
            let target = (eff_gap as f64 * gap_fraction as f64) as u64;
            let floor = (m.host_memory_total as f64 * floor_ratio as f64) as u64;
            let cap = (m.host_memory_total as f64 * cap_ratio as f64) as u64;
            let cap = cap.max(floor); // guarantee cap >= floor
            ReclamationAction::GradualReclaim {
                bytes: target.clamp(floor, cap),
            }
        } else {
            ReclamationAction::NoAction
        }
    }

    fn cooldown_elapsed(last: Option<Instant>, cooldown: Duration, now: Instant) -> bool {
        match last {
            Some(t) => now.duration_since(t) >= cooldown,
            None => true,
        }
    }

    pub fn get_diagnostics(&self) -> String {
        if let Some(m) = self.metrics_history.back() {
            let pressure = self.calculate_pressure_level();
            let score = self.score_metrics(m);
            let sustained = self.check_sustained_pressure();
            let eff_gap = m.gap.saturating_sub(self.config.baseline_gap);
            format!(
                "Pressure: {:?} (score={:.2}, sustained={}/3), \
                 Gap: {:.2}GB (eff {:.2}GB, base {:.1}GB), \
                 Host: {:.1}% used, Cache: {:.2}GB ({:.0}%)",
                pressure,
                score,
                sustained,
                m.gap as f64 / GB as f64,
                eff_gap as f64 / GB as f64,
                self.config.baseline_gap as f64 / GB as f64,
                if m.host_memory_total > 0 {
                    (1.0 - m.host_memory_avail as f64 / m.host_memory_total as f64) * 100.0
                } else {
                    0.0
                },
                m.guest_file_cache as f64 / GB as f64,
                if m.guest_resident > 0 {
                    m.guest_file_cache as f64 / m.guest_resident as f64 * 100.0
                } else {
                    0.0
                },
            )
        } else {
            "No metrics available".to_string()
        }
    }
}

fn score_to_level(score: f32) -> PressureLevel {
    if score >= 3.5 {
        PressureLevel::Critical
    } else if score >= 2.5 {
        PressureLevel::Heavy
    } else if score >= 1.5 {
        PressureLevel::Moderate
    } else if score >= 0.5 {
        PressureLevel::Mild
    } else {
        PressureLevel::None
    }
}

const GB: u64 = 1024 * 1024 * 1024;

// =========================================================================
// Guest-local reclamation (no host connection required)
//
// The guest can only see /proc/meminfo and local CPU/IO.  It does NOT know
// about vmmem RSS, host memory pressure, or .wslconfig limits.  So the
// local algorithm is deliberately **conservative** — it only reclaims when
// the file-cache ratio is clearly excessive and the system is idle.
//
// When the host connection is active the host has a much broader view and
// should be the authority.  The local loop **yields** to host commands by
// checking a shared "last host command" timestamp.
// =========================================================================

/// Guest-local metrics snapshot (everything readable from /proc).
#[derive(Debug, Clone)]
pub struct GuestLocalMetrics {
    pub mem_total: u64,
    pub mem_available: u64,
    pub file_cache: u64,
    pub resident: u64,
    pub cpu_percent: f32,
    pub io_rate: f32,
}

/// All thresholds are ratios — no absolute byte counts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GuestLocalConfig {
    /// File-cache / mem_total above which we start reclaiming.
    pub cache_ratio_moderate: f32, // 0.50
    pub cache_ratio_heavy: f32, // 0.70

    /// MemAvailable / MemTotal below which we treat as pressure.
    pub avail_ratio_low: f32, // 0.15

    /// Idle detection (same semantics as host-side config).
    pub cpu_idle: f32, // 0.05
    pub io_idle: f32, // 10.0 MB/s

    /// Fraction of reclaimable cache to actually reclaim per tick.
    pub reclaim_fraction_moderate: f32, // 0.10
    pub reclaim_fraction_heavy: f32, // 0.25

    /// Reclaim size limits as fractions of mem_total.
    pub reclaim_floor_ratio: f32, // 0.002
    pub reclaim_cap_ratio: f32, // 0.010

    /// Number of consecutive ticks needed before acting.
    pub sustained_ticks: usize, // 3

    /// Cooldown between local reclaim actions.
    pub cooldown: Duration, // 15 s

    /// How long after a host command we stay silent.
    pub host_defer: Duration, // 30 s

    /// Interval between local check ticks.
    pub check_interval: Duration, // 5 s
}

impl Default for GuestLocalConfig {
    fn default() -> Self {
        Self {
            cache_ratio_moderate: 0.50,
            cache_ratio_heavy: 0.70,
            avail_ratio_low: 0.15,
            cpu_idle: 0.05,
            io_idle: 10.0,
            reclaim_fraction_moderate: 0.10,
            reclaim_fraction_heavy: 0.25,
            reclaim_floor_ratio: 0.002,
            reclaim_cap_ratio: 0.010,
            sustained_ticks: 3,
            cooldown: Duration::from_secs(15),
            host_defer: Duration::from_secs(30),
            check_interval: Duration::from_secs(5),
        }
    }
}

#[derive(Debug)]
pub enum GuestLocalAction {
    Nothing,
    Reclaim { bytes: u64 },
}

pub struct GuestLocalReclaimer {
    config: GuestLocalConfig,
    history: VecDeque<GuestLocalMetrics>,
    last_action: Option<Instant>,
}

impl GuestLocalReclaimer {
    pub fn new(config: GuestLocalConfig) -> Self {
        Self {
            config,
            history: VecDeque::with_capacity(10),
            last_action: None,
        }
    }

    pub fn config(&self) -> &GuestLocalConfig {
        &self.config
    }

    pub fn push(&mut self, m: GuestLocalMetrics) {
        self.history.push_back(m);
        if self.history.len() > 10 {
            self.history.pop_front();
        }
    }

    pub fn decide(&mut self, host_last_cmd: Option<Instant>) -> GuestLocalAction {
        let now = Instant::now();

        // Yield to host: if the host sent a command recently, do nothing.
        if let Some(t) = host_last_cmd {
            if now.duration_since(t) < self.config.host_defer {
                return GuestLocalAction::Nothing;
            }
        }

        // Cooldown gate.
        if let Some(t) = self.last_action {
            if now.duration_since(t) < self.config.cooldown {
                return GuestLocalAction::Nothing;
            }
        }

        let m = match self.history.back() {
            Some(m) => m,
            None => return GuestLocalAction::Nothing,
        };

        let cache_ratio = if m.mem_total > 0 {
            m.file_cache as f64 / m.mem_total as f64
        } else {
            0.0
        };
        let avail_ratio = if m.mem_total > 0 {
            m.mem_available as f64 / m.mem_total as f64
        } else {
            1.0
        };
        let idle = m.cpu_percent < self.config.cpu_idle && m.io_rate < self.config.io_idle;

        // Determine severity.
        let heavy = cache_ratio >= self.config.cache_ratio_heavy as f64
            || (cache_ratio >= self.config.cache_ratio_moderate as f64
                && avail_ratio < self.config.avail_ratio_low as f64);

        let moderate = !heavy && cache_ratio >= self.config.cache_ratio_moderate as f64;

        if !heavy && !moderate {
            return GuestLocalAction::Nothing;
        }

        // Only act on idle system for moderate; heavy acts regardless.
        if moderate && !idle {
            return GuestLocalAction::Nothing;
        }

        // Require sustained pressure.
        let n = self.config.sustained_ticks;
        if self.history.len() < n {
            return GuestLocalAction::Nothing;
        }
        let sustained = self.history.iter().rev().take(n).all(|h| {
            h.mem_total > 0
                && h.file_cache as f64 / h.mem_total as f64
                    >= self.config.cache_ratio_moderate as f64
        });
        if !sustained {
            return GuestLocalAction::Nothing;
        }

        // Size the reclaim: fraction of reclaimable file cache.
        let fraction = if heavy {
            self.config.reclaim_fraction_heavy
        } else {
            self.config.reclaim_fraction_moderate
        };
        let target = (m.file_cache as f64 * fraction as f64) as u64;
        let floor = (m.mem_total as f64 * self.config.reclaim_floor_ratio as f64) as u64;
        let cap = (m.mem_total as f64 * self.config.reclaim_cap_ratio as f64) as u64;
        let cap = cap.max(floor);
        let bytes = target.clamp(floor, cap);

        self.last_action = Some(now);
        GuestLocalAction::Reclaim { bytes }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build metrics for a host with `host_gb` total RAM.
    fn make_metrics(
        host_gb: f64,
        gap_gb: f64,
        host_used_pct: f64,
        wslconfig_gb: Option<f64>,
        cache_pct: f64,
        guest_cpu: f32,
        guest_io: f32,
    ) -> SystemMetrics {
        let host_total = (host_gb * GB as f64) as u64;
        let host_avail = ((1.0 - host_used_pct / 100.0) * host_total as f64) as u64;
        let guest_resident: u64 = 8 * GB;
        let guest_file_cache = (guest_resident as f64 * cache_pct / 100.0) as u64;
        let vmmem_rss = guest_resident + (gap_gb * GB as f64) as u64;

        SystemMetrics {
            vmmem_rss,
            host_memory_total: host_total,
            host_memory_avail: host_avail,
            wslconfig_memory_limit: wslconfig_gb.map(|g| (g * GB as f64) as u64),
            guest_resident,
            guest_file_cache,
            guest_cpu_percent: guest_cpu,
            guest_io_rate: guest_io,
            gap: (gap_gb * GB as f64) as u64,
        }
    }

    #[test]
    fn test_baseline_subtracted() {
        // gap=2.5 GB on 32 GB host → effective gap = 0.5 GB after 2 GB baseline
        // ratio = 0.5/32 = 0.016 → gap_score=1, weighted 0.3
        // all other dimensions calm → total 0.3 < 0.5 → None
        // WITHOUT baseline: raw 2.5/32=0.078 → gap_score=3 → would be Moderate
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        r.push_metrics(make_metrics(32.0, 2.5, 50.0, None, 30.0, 0.1, 1.0));
        assert_eq!(
            r.calculate_pressure_level(),
            PressureLevel::None,
            "baseline subtraction should suppress pressure from WSL2 overhead"
        );
    }

    #[test]
    fn test_gap_within_baseline_is_none() {
        // gap=1.5 GB < baseline(2 GB) → effective gap = 0 → None
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        r.push_metrics(make_metrics(32.0, 1.5, 50.0, None, 30.0, 0.1, 1.0));
        assert_eq!(r.calculate_pressure_level(), PressureLevel::None);
    }

    #[test]
    fn test_moderate_on_32gb_host() {
        // gap=4 GB (eff=2 GB), host 82%, cache 65%
        // gap ratio = 2/32 = 0.0625 → ≥0.03 → score=2
        // host 82% → score=2, cache 65% → score=2
        // total = 2×0.3 + 2×0.25 + 0 + 2×0.2 = 0.6+0.5+0.4 = 1.5 → Moderate
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        r.push_metrics(make_metrics(32.0, 4.0, 82.0, None, 65.0, 0.1, 1.0));
        assert_eq!(r.calculate_pressure_level(), PressureLevel::Moderate);
    }

    #[test]
    fn test_scales_with_host_size() {
        // Same absolute gap of 4 GB, but on a 64 GB host
        // eff = 2 GB, ratio = 2/64 = 0.03125 → ≥0.03 moderate, same tier
        // But on 128 GB host: ratio = 2/128 = 0.0156 → only mild
        let mut r64 = ElasticReclaimer::new(ReclamationConfig::default());
        r64.push_metrics(make_metrics(64.0, 4.0, 82.0, None, 65.0, 0.1, 1.0));

        let mut r128 = ElasticReclaimer::new(ReclamationConfig::default());
        r128.push_metrics(make_metrics(128.0, 4.0, 50.0, None, 30.0, 0.1, 1.0));

        assert!(r64.calculate_pressure_level() >= PressureLevel::Moderate);
        assert!(r128.calculate_pressure_level() <= PressureLevel::Mild);
    }

    #[test]
    fn test_heavy_pressure() {
        // 32 GB host, gap=6 GB (eff=4, ratio=0.125→≥0.06 Heavy=3), host 92%, wslconfig 10 GB, cache 85%
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        r.push_metrics(make_metrics(32.0, 6.0, 92.0, Some(10.0), 85.0, 0.1, 1.0));
        assert!(
            r.calculate_pressure_level() >= PressureLevel::Heavy,
            "got {:?}",
            r.calculate_pressure_level()
        );
    }

    #[test]
    fn test_critical_reachable() {
        // 32 GB host, gap=8 GB (eff=6, ratio=0.1875→Critical),
        // host 96%, wslconfig 12 GB (vmmem=16/12=1.33→4), cache 92%, sustained
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        let m = make_metrics(32.0, 8.0, 96.0, Some(12.0), 92.0, 0.01, 0.5);
        r.push_metrics(m.clone());
        r.push_metrics(m.clone());
        r.push_metrics(m);
        assert_eq!(
            r.calculate_pressure_level(),
            PressureLevel::Critical,
            "Critical must be reachable"
        );
    }

    #[test]
    fn test_compact_on_sustained_heavy() {
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        // 3 windows: 32 GB host, gap=6 GB, host 92%, cache 85% → Heavy + sustained≥2
        for _ in 0..3 {
            r.push_metrics(make_metrics(32.0, 6.0, 92.0, None, 85.0, 0.1, 1.0));
        }
        let action = r.decide_action();
        assert!(
            matches!(action, ReclamationAction::Compact),
            "expected Compact on sustained heavy, got {:?}",
            action
        );
    }

    #[test]
    fn test_reclaim_scales_with_host_size() {
        // On 32 GB host, moderate reclaim cap = 32×0.008 = 256 MB
        // On 64 GB host, moderate reclaim cap = 64×0.008 = 512 MB
        let cfg = ReclamationConfig::default();

        let mut r32 = ElasticReclaimer::new(cfg.clone());
        r32.push_metrics(make_metrics(32.0, 4.0, 82.0, None, 65.0, 0.1, 1.0));
        let a32 = r32.decide_action();

        let mut r64 = ElasticReclaimer::new(cfg);
        r64.push_metrics(make_metrics(64.0, 6.0, 82.0, None, 65.0, 0.1, 1.0));
        let a64 = r64.decide_action();

        if let (
            ReclamationAction::GradualReclaim { bytes: b32 },
            ReclamationAction::GradualReclaim { bytes: b64 },
        ) = (&a32, &a64)
        {
            assert!(
                b64 > b32,
                "64 GB host should reclaim more: {} vs {}",
                b64,
                b32
            );
        }
    }

    #[test]
    fn test_sustained_multi_dimensional() {
        let mut r = ElasticReclaimer::new(ReclamationConfig::default());
        // gap eff=4/32=0.125≥0.03 ✓, host 85%>80% ✓, cache 75%>60% ✓ → sustained=3
        for _ in 0..3 {
            r.push_metrics(make_metrics(32.0, 6.0, 85.0, None, 75.0, 0.1, 1.0));
        }
        assert_eq!(r.check_sustained_pressure(), 3);
    }

    #[test]
    fn test_default_baseline_exists() {
        let cfg = ReclamationConfig::default();
        assert_eq!(cfg.baseline_gap, 2 * GB);
    }

    // ---------------------------------------------------------------
    // GuestLocalReclaimer tests
    // ---------------------------------------------------------------

    fn make_local(mem_gb: f64, cache_pct: f64, avail_pct: f64) -> GuestLocalMetrics {
        let total = (mem_gb * GB as f64) as u64;
        GuestLocalMetrics {
            mem_total: total,
            mem_available: (total as f64 * avail_pct / 100.0) as u64,
            file_cache: (total as f64 * cache_pct / 100.0) as u64,
            resident: (total as f64 * (1.0 - avail_pct / 100.0)) as u64,
            cpu_percent: 0.01,
            io_rate: 0.5,
        }
    }

    #[test]
    fn test_local_no_action_low_cache() {
        let mut r = GuestLocalReclaimer::new(GuestLocalConfig::default());
        for _ in 0..5 {
            r.push(make_local(8.0, 30.0, 50.0));
        }
        assert!(matches!(r.decide(None), GuestLocalAction::Nothing));
    }

    #[test]
    fn test_local_reclaim_high_cache_sustained() {
        let mut r = GuestLocalReclaimer::new(GuestLocalConfig::default());
        // cache 75% > heavy threshold (70%), sustained 3+ ticks, idle
        for _ in 0..4 {
            r.push(make_local(8.0, 75.0, 40.0));
        }
        let action = r.decide(None);
        assert!(
            matches!(action, GuestLocalAction::Reclaim { bytes } if bytes > 0),
            "expected Reclaim, got {:?}",
            action
        );
    }

    #[test]
    fn test_local_defers_to_host() {
        let mut r = GuestLocalReclaimer::new(GuestLocalConfig::default());
        for _ in 0..4 {
            r.push(make_local(8.0, 75.0, 40.0));
        }
        // Host sent a command just now → local should stay silent.
        let action = r.decide(Some(Instant::now()));
        assert!(
            matches!(action, GuestLocalAction::Nothing),
            "should defer to host"
        );
    }

    #[test]
    fn test_local_scales_with_mem_size() {
        // 8 GB vs 32 GB — reclaim cap is mem_total × 0.01
        let mut r8 = GuestLocalReclaimer::new(GuestLocalConfig::default());
        for _ in 0..4 {
            r8.push(make_local(8.0, 75.0, 40.0));
        }
        let a8 = r8.decide(None);

        let mut r32 = GuestLocalReclaimer::new(GuestLocalConfig::default());
        for _ in 0..4 {
            r32.push(make_local(32.0, 75.0, 40.0));
        }
        let a32 = r32.decide(None);

        if let (GuestLocalAction::Reclaim { bytes: b8 }, GuestLocalAction::Reclaim { bytes: b32 }) =
            (&a8, &a32)
        {
            assert!(b32 > b8, "32 GB should reclaim more: {} vs {}", b32, b8);
        } else {
            panic!("expected both Reclaim, got {:?} / {:?}", a8, a32);
        }
    }

    #[test]
    fn test_local_moderate_requires_idle() {
        let cfg = GuestLocalConfig::default();
        let mut r = GuestLocalReclaimer::new(cfg);
        // cache 55% → moderate (≥50%, <70%), but NOT idle
        for _ in 0..4 {
            r.push(GuestLocalMetrics {
                mem_total: 8 * GB,
                mem_available: 4 * GB,
                file_cache: (8.0 * 0.55 * GB as f64) as u64,
                resident: 5 * GB,
                cpu_percent: 50.0,
                io_rate: 200.0,
            });
        }
        assert!(matches!(r.decide(None), GuestLocalAction::Nothing));
    }
}
