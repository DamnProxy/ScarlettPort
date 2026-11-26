//! What the performance page shows: frame times, memory, and which speed
//! switches the environment has on.

use std::collections::VecDeque;

/// Frames of history the graph keeps.
pub const HISTORY: usize = 240;

/// Present-to-present times, newest last.
#[derive(Default)]
pub struct Perf {
    ms: VecDeque<f32>,
    /// Present times of the last second, nanoseconds.
    recent: VecDeque<u64>,
}

impl Perf {
    /// A frame was presented at `now` (nanoseconds on any steady clock).
    pub fn present(&mut self, now: u64) {
        if let Some(&last) = self.recent.back()
            && now > last
        {
            self.ms.push_back((now - last) as f32 / 1e6);
            if self.ms.len() > HISTORY {
                self.ms.pop_front();
            }
        }
        self.recent.push_back(now);
        while self.recent.len() > 2 && now.saturating_sub(self.recent[0]) > 1_000_000_000 {
            self.recent.pop_front();
        }
    }

    /// Frames a second over the last second (0 with fewer than two frames).
    pub fn fps(&self) -> f32 {
        let (Some(&first), Some(&last)) = (self.recent.front(), self.recent.back()) else {
            return 0.0;
        };
        if self.recent.len() < 2 || last <= first {
            return 0.0;
        }
        (self.recent.len() - 1) as f32 * 1e9 / (last - first) as f32
    }

    /// The mean frame time of the last second, ms.
    pub fn frame_ms(&self) -> f32 {
        match self.fps() {
            f if f > 0.0 => 1000.0 / f,
            _ => 0.0,
        }
    }

    /// The slowest frame in the last `n`, ms.
    pub fn worst_ms(&self, n: usize) -> f32 {
        self.ms.iter().rev().take(n).copied().fold(0.0, f32::max)
    }

    /// The frame time 99% of the last `HISTORY` frames beat, ms.
    pub fn p99_ms(&self) -> f32 {
        if self.ms.is_empty() {
            return 0.0;
        }
        let mut sorted: Vec<f32> = self.ms.iter().copied().collect();
        sorted.sort_by(f32::total_cmp);
        sorted[((sorted.len() - 1) as f32 * 0.99).round() as usize]
    }

    /// The last `n` frame times, oldest first.
    pub fn samples(&self, n: usize) -> Vec<f32> {
        let skip = self.ms.len().saturating_sub(n);
        self.ms.iter().skip(skip).copied().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.ms.is_empty()
    }
}

/// Memory, as the host reports it now (bytes; 0 where unknown).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SysInfo {
    /// This process's working set and private bytes.
    pub ram_working_set: u64,
    pub ram_private: u64,
    /// The PC's memory: free and all of it.
    pub ram_available: u64,
    pub ram_total: u64,
    /// The card's own memory: used by this process and its budget.
    pub vram_used: u64,
    pub vram_budget: u64,
    /// The PC's memory the card shares: used by this process and its budget.
    pub shared_used: u64,
    pub shared_budget: u64,
    pub vram_known: bool,
}

/// How a speed switch is changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// In the overlay, at once.
    Live,
    /// By the environment or the config file, at the next start.
    Restart,
}

/// The environment switches that make the runtime faster (or trade one
/// cost for another), as the performance page lists them. They are read at
/// start, so changing one needs a restart; the pacing ones are read live
/// from the pacing state instead.
pub const SPEED_SWITCHES: &[&str] = &[
    "VICTORY_PSO_SHARE",
    "VICTORY_PSO_CACHE",
    "VICTORY_PRECOMPILED_PSO",
    "VICTORY_SHADER_OPT",
    "VICTORY_GROUP_SYNC",
    "VICTORY_JOIN_COPIES",
    "VICTORY_COPY_FAST",
    "VICTORY_FAST_HASH",
    "VICTORY_DIRECT_WRITES",
    "VICTORY_CUT_WAITS",
    "VICTORY_CUT_WRITES",
    "VICTORY_LATE_WAITS",
    "VICTORY_SPLIT_WAITS",
    "VICTORY_QUEUE_WAIT",
    "VICTORY_BARRIER_MERGE",
    "VICTORY_INDIRECT_MERGE",
    "VICTORY_INDIRECT_WINDOW",
    "VICTORY_INPLACE",
    "VICTORY_LAZY_TEXTURES",
    "VICTORY_TABLED_TEXTURES",
    "VICTORY_HEAP_TEXTURES",
    "VICTORY_TEXTURE_NO_WAIT",
    "VICTORY_MAPPING_RANGES",
    "VICTORY_LIST_POOL",
    "VICTORY_SCRATCH_POOL",
    "VICTORY_CARD_REHASH_RUNS",
    "VICTORY_WINDOW_TAILS",
    "VICTORY_DEPTH_TWINS",
    "VICTORY_DS_UNBUFFERED",
    "VICTORY_EARLY_WORKERS",
    "VICTORY_REBAR",
    "VICTORY_RENDER_SCALE",
];

/// One row of the switch list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwitchRow {
    pub name: String,
    pub on: bool,
    /// What the variable says ("-" when unset).
    pub value: String,
    pub change: Change,
}

/// Whether a variable's value means on: set, and not a spelling of off.
pub fn means_on(value: &str) -> bool {
    !matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "off" | "no"
    )
}

/// The switch rows for a variable lookup (`SCARLETT_` spellings count, as
/// everywhere), with the pacing switches from `pace` (their state now).
pub fn switch_rows(
    get: &dyn Fn(&str) -> Option<String>,
    pace: &[(&str, String, bool, Change)],
) -> Vec<SwitchRow> {
    let mut rows: Vec<SwitchRow> = pace
        .iter()
        .map(|(name, value, on, change)| SwitchRow {
            name: (*name).to_string(),
            on: *on,
            value: value.clone(),
            change: *change,
        })
        .collect();
    for &name in SPEED_SWITCHES {
        let value = get(name);
        let on = value.as_deref().is_some_and(means_on);
        let shown = match value {
            Some(v) if v.chars().count() > 12 => "set".to_string(),
            Some(v) if v.trim().is_empty() => "-".to_string(),
            Some(v) => v.trim().to_string(),
            None => "-".to_string(),
        };
        rows.push(SwitchRow {
            name: name.to_string(),
            on,
            value: shown,
            change: Change::Restart,
        });
    }
    rows
}

/// Bytes as a short size: `812 MB`, `1.4 GB`.
pub fn size(bytes: u64) -> String {
    let mb = bytes as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 {
        format!("{:.1} GB", mb / 1024.0)
    } else {
        format!("{mb:.0} MB")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_and_percentiles_follow_the_presents() {
        let mut p = Perf::default();
        assert_eq!(p.fps(), 0.0);
        // 60 frames of 16.667 ms, then one of 100 ms.
        let mut t = 0u64;
        for _ in 0..60 {
            p.present(t);
            t += 16_666_667;
        }
        p.present(t + 83_333_333);
        assert!((p.worst_ms(5) - 100.0).abs() < 0.1);
        assert!(p.fps() > 50.0 && p.fps() < 65.0, "{}", p.fps());
        assert!((p.p99_ms() - 100.0).abs() < 0.2 || (p.p99_ms() - 16.7).abs() < 0.2);
        assert_eq!(p.samples(10).len(), 10);
        assert!(p.samples(10_000).len() <= HISTORY);
    }

    #[test]
    fn history_is_bounded_and_the_second_window_slides() {
        let mut p = Perf::default();
        for i in 0..1000u64 {
            p.present(i * 10_000_000);
        }
        assert_eq!(p.samples(10_000).len(), HISTORY);
        assert!((p.fps() - 100.0).abs() < 1.0);
        assert!((p.frame_ms() - 10.0).abs() < 0.2);
    }

    #[test]
    fn switches_read_as_on_unless_off_spelled() {
        for off in ["", "0", "false", "OFF", " no "] {
            assert!(!means_on(off), "{off:?}");
        }
        for on in ["1", "3", "true", "yes", "C:\\path"] {
            assert!(means_on(on), "{on:?}");
        }
    }

    #[test]
    fn rows_list_the_pacing_switches_first_and_every_speed_switch() {
        let get = |n: &str| match n {
            "VICTORY_GROUP_SYNC" => Some("3".to_string()),
            "VICTORY_PSO_CACHE" => Some("0".to_string()),
            "VICTORY_PSO_SHARE" => Some("C:\\a\\very\\long\\path".to_string()),
            _ => None,
        };
        let pace = [("VICTORY_PACE", "on".to_string(), true, Change::Live)];
        let rows = switch_rows(&get, &pace);
        assert_eq!(rows[0].name, "VICTORY_PACE");
        assert_eq!(rows[0].change, Change::Live);
        assert_eq!(rows.len(), SPEED_SWITCHES.len() + 1);
        let row = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert!(row("VICTORY_GROUP_SYNC").on);
        assert_eq!(row("VICTORY_GROUP_SYNC").value, "3");
        assert!(!row("VICTORY_PSO_CACHE").on);
        assert_eq!(row("VICTORY_PSO_CACHE").value, "0");
        assert_eq!(row("VICTORY_PSO_SHARE").value, "set");
        assert!(row("VICTORY_PSO_SHARE").on);
        assert_eq!(row("VICTORY_JOIN_COPIES").value, "-");
        assert_eq!(row("VICTORY_JOIN_COPIES").change, Change::Restart);
    }

    #[test]
    fn sizes_are_short() {
        assert_eq!(size(812 << 20), "812 MB");
        assert_eq!(size(3 << 29), "1.5 GB");
    }
}
