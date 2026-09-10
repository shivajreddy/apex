//! Frecency: launch history that ranks recent and frequent picks above
//! merely well-matching ones.
//!
//! Each entry keeps one decaying weight rather than a list of timestamps: on
//! launch the stored weight is decayed to now and 1.0 added, so a single f32
//! plus a timestamp captures both frequency *and* recency. Comparing two
//! entries then costs one `exp2` each.
//!
//! The file lives under `%LOCALAPPDATA%`, deliberately not beside
//! `config.toml`. This is machine-local state that rewrites on every launch,
//! whereas the config is user-owned and commonly symlinked into a dotfiles
//! repo - writing churn there would leave the repo permanently dirty.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Time for a launch's contribution to lose half its weight.
const HALF_LIFE_SECS: f32 = 14.0 * 24.0 * 60.0 * 60.0;

/// Largest bonus frecency may add to a plugin's relevance score. Sized to
/// outweigh a single word-start bonus (32 in [`crate::fuzzy`]) so a well-used
/// app can beat a slightly better match, while staying far below the search
/// plugin's alias boost (2000) so an exact alias always wins outright.
const WEIGHT: f32 = 40.0;

/// Decayed weight at which an entry earns half of [`WEIGHT`]. Raw weights
/// grow without bound, so the bonus saturates instead of scaling linearly -
/// otherwise heavy use would eventually swamp match quality entirely.
const HALF_BONUS_AT: f32 = 4.0;

/// Entries decaying below this are dropped on save, which also garbage
/// collects apps that have since been uninstalled.
const PRUNE_BELOW: f32 = 0.05;

const HEADER: &str = "# apex frecency - machine-local launch history, safe to delete";

/// Weights are stored as fixed-point thousandths rather than decimals.
/// Rust's float formatting and parsing drag in the Dragon4 and dec2flt
/// machinery, which costs tens of KB of binary for a value that never needs
/// more than three decimal places.
const SCALE: f32 = 1000.0;

struct Entry {
    weight: f32,
    /// Unix seconds of the launch that produced `weight`.
    last: u64,
}

impl Entry {
    fn decayed(&self, now: u64) -> f32 {
        // saturating_sub: a clock that jumped backwards must not inflate.
        let dt = now.saturating_sub(self.last) as f32;
        self.weight * (-dt / HALF_LIFE_SECS).exp2()
    }
}

/// Launch history keyed by plugin id, then by payload.
///
/// Nested rather than keyed on a `(String, String)` tuple so lookups take
/// `&str` and allocate nothing: [`Frecency::bonus`] runs for every result on
/// every keystroke.
#[derive(Default)]
pub struct Frecency {
    entries: HashMap<String, HashMap<String, Entry>>,
}

/// Current unix time in seconds. Passed explicitly into the methods below so
/// the whole module stays testable without touching the clock.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn path() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("apex").join("frecency.tsv"))
}

impl Frecency {
    pub fn load() -> Self {
        let text = path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        Self::parse(&text)
    }

    /// Bonus to add to a result's relevance score. 0 when never launched.
    pub fn bonus(&self, plugin: &str, payload: &str, now: u64) -> i32 {
        let Some(entry) = self.entries.get(plugin).and_then(|m| m.get(payload)) else {
            return 0;
        };
        let w = entry.decayed(now);
        (WEIGHT * w / (w + HALF_BONUS_AT)) as i32
    }

    /// Note a launch: decay what is there to `now`, then add one full unit.
    pub fn record(&mut self, plugin: &str, payload: &str, now: u64) {
        let by_payload = self.entries.entry(plugin.to_string()).or_default();
        match by_payload.get_mut(payload) {
            Some(e) => {
                e.weight = e.decayed(now) + 1.0;
                e.last = now;
            }
            None => {
                by_payload.insert(
                    payload.to_string(),
                    Entry {
                        weight: 1.0,
                        last: now,
                    },
                );
            }
        }
    }

    /// The `n` most frecent `(plugin, payload)` pairs, best first. Used to
    /// fill the result list while the query is empty.
    pub fn top(&self, n: usize, now: u64) -> Vec<(&str, &str)> {
        let mut ranked: Vec<(f32, &str, &str)> = self
            .entries
            .iter()
            .flat_map(|(plugin, m)| {
                m.iter()
                    .map(move |(payload, e)| (e.decayed(now), plugin.as_str(), payload.as_str()))
            })
            .collect();
        // Tie-break on payload: HashMap iteration order is randomised, so
        // without it equally-weighted entries would reshuffle every summon.
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.2.cmp(b.2)));
        ranked.truncate(n);
        ranked.into_iter().map(|(_, p, k)| (p, k)).collect()
    }

    pub fn save(&mut self, now: u64) {
        self.prune(now);
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, self.to_text());
    }

    fn prune(&mut self, now: u64) {
        for m in self.entries.values_mut() {
            m.retain(|_, e| e.decayed(now) >= PRUNE_BELOW);
        }
        self.entries.retain(|_, m| !m.is_empty());
    }

    fn parse(text: &str) -> Self {
        let mut entries: HashMap<String, HashMap<String, Entry>> = HashMap::new();
        for line in text.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // splitn(4): payload comes last so a stray tab inside it cannot
            // shift the numeric fields.
            let mut parts = line.splitn(4, '\t');
            let (Some(plugin), Some(weight), Some(last), Some(payload)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(milli), Ok(last)) = (weight.parse::<u64>(), last.parse::<u64>()) else {
                continue;
            };
            let weight = milli as f32 / SCALE;
            if weight <= 0.0 || plugin.is_empty() || payload.is_empty() {
                continue;
            }
            entries
                .entry(plugin.to_string())
                .or_default()
                .insert(payload.to_string(), Entry { weight, last });
        }
        Self { entries }
    }

    fn to_text(&self) -> String {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .flat_map(|(plugin, m)| {
                m.iter().map(move |(payload, e)| {
                    let milli = (e.weight * SCALE) as u64;
                    format!("{plugin}\t{milli}\t{}\t{payload}", e.last)
                })
            })
            .collect();
        lines.sort(); // stable order, so the file diffs cleanly
        let mut out = String::with_capacity(HEADER.len() + lines.len() * 48);
        out.push_str(HEADER);
        out.push('\n');
        for l in lines {
            out.push_str(&l);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;
    const T0: u64 = 1_700_000_000;

    fn weight_of(f: &Frecency, plugin: &str, payload: &str, now: u64) -> f32 {
        f.entries[plugin][payload].decayed(now)
    }

    #[test]
    fn unknown_entries_score_zero() {
        let f = Frecency::default();
        assert_eq!(f.bonus("search", "nope", T0), 0);
    }

    #[test]
    fn repeated_launches_accumulate() {
        let mut f = Frecency::default();
        f.record("search", "a", T0);
        let one = weight_of(&f, "search", "a", T0);
        f.record("search", "a", T0);
        f.record("search", "a", T0);
        let three = weight_of(&f, "search", "a", T0);
        assert!(three > one, "{three} should exceed {one}");
        assert!(f.bonus("search", "a", T0) > 0);
    }

    #[test]
    fn weight_halves_over_one_half_life() {
        let mut f = Frecency::default();
        f.record("search", "a", T0);
        let fresh = weight_of(&f, "search", "a", T0);
        let aged = weight_of(&f, "search", "a", T0 + 14 * DAY);
        assert!(
            (aged - fresh / 2.0).abs() < 1e-4,
            "expected {} got {aged}",
            fresh / 2.0
        );
    }

    #[test]
    fn bonus_is_bounded_by_weight() {
        let mut f = Frecency::default();
        for _ in 0..10_000 {
            f.record("search", "a", T0);
        }
        assert!(f.bonus("search", "a", T0) <= WEIGHT as i32);
    }

    #[test]
    fn bonus_grows_with_use() {
        let mut f = Frecency::default();
        f.record("search", "once", T0);
        for _ in 0..10 {
            f.record("search", "often", T0);
        }
        assert!(f.bonus("search", "often", T0) > f.bonus("search", "once", T0));
    }

    #[test]
    fn recent_beats_frequent_but_stale() {
        let mut f = Frecency::default();
        // 100 launches two months ago...
        let long_ago = T0 - 60 * DAY;
        for _ in 0..100 {
            f.record("search", "stale", long_ago);
        }
        // ...loses to 20 launches today.
        for _ in 0..20 {
            f.record("search", "fresh", T0);
        }
        let ranked = f.top(2, T0);
        assert_eq!(ranked[0], ("search", "fresh"));
        assert_eq!(ranked[1], ("search", "stale"));
    }

    #[test]
    fn top_is_deterministic_for_equal_weights() {
        let mut f = Frecency::default();
        for k in ["c", "a", "b"] {
            f.record("search", k, T0);
        }
        assert_eq!(
            f.top(3, T0),
            vec![("search", "a"), ("search", "b"), ("search", "c")]
        );
    }

    #[test]
    fn top_spans_plugins_and_respects_n() {
        let mut f = Frecency::default();
        f.record("search", "app", T0);
        for _ in 0..5 {
            f.record("quicklinks", "link", T0);
        }
        assert_eq!(f.top(1, T0), vec![("quicklinks", "link")]);
        assert_eq!(f.top(9, T0).len(), 2);
    }

    #[test]
    fn round_trips_through_text() {
        let mut f = Frecency::default();
        f.record("search", "Some.App_8wek\\App", T0);
        f.record("search", "Other", T0 - DAY);
        let back = Frecency::parse(&f.to_text());
        assert_eq!(back.entries["search"].len(), 2);
        assert!(
            (weight_of(&back, "search", "Other", T0) - weight_of(&f, "search", "Other", T0)).abs()
                < 1e-3
        );
    }

    #[test]
    fn parse_skips_comments_and_junk() {
        let text = "# header\n\
                    search\t1000\t100\tgood\n\
                    search\tnotanumber\t100\tbad\n\
                    search\t1000\tnotatime\tbad2\n\
                    missing-fields\n\
                    search\t-3000\t100\tnegative\n\
                    search\t1.5\t100\tdecimal-is-not-fixed-point\n\
                    search\t0\t100\tzero\n";
        let f = Frecency::parse(text);
        assert_eq!(f.entries["search"].len(), 1);
        assert!(f.entries["search"].contains_key("good"));
        assert_eq!(f.entries["search"]["good"].weight, 1.0);
    }

    #[test]
    fn fixed_point_survives_a_save_load_cycle() {
        let mut f = Frecency::default();
        for _ in 0..7 {
            f.record("search", "a", T0);
        }
        let before = weight_of(&f, "search", "a", T0);
        let after = weight_of(&Frecency::parse(&f.to_text()), "search", "a", T0);
        // Thousandths are far finer than the bonus curve can resolve.
        assert!((before - after).abs() < 1.0 / SCALE, "{before} vs {after}");
    }

    #[test]
    fn prune_drops_decayed_entries_and_empty_plugins() {
        let mut f = Frecency::default();
        f.record("search", "ancient", T0);
        f.record("search", "recent", T0 + 200 * DAY);
        f.prune(T0 + 200 * DAY);
        assert!(!f.entries["search"].contains_key("ancient"));
        assert!(f.entries["search"].contains_key("recent"));

        let mut g = Frecency::default();
        g.record("gone", "x", T0);
        g.prune(T0 + 400 * DAY);
        assert!(g.entries.is_empty(), "empty plugin map should be removed");
    }
}
