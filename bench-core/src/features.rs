//! Derived features — the only thing Bench ever writes to a logic cell.
//!
//! **Never assert samples.** A 4096-sample frame would be 4096 facts per
//! capture, and live panel queries cap at 200 rows anyway. What belongs in the
//! cell is what a rule can reason about: dominant frequency, THD, RMS, regime
//! label, and the link to the previous capture.
//!
//! That restraint is what turns Bench from a scope into a test bench. Facts at
//! this granularity support rules like *"fails if THD > 3% or dominant drifts
//! more than 2% from nominal"*, evaluated at zero token cost.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Measurements extracted from one captured frame.
///
/// Every field is `Option` because features come from different steps of a
/// chain: a chain without `signal.spectral` has no dominant frequency, and
/// inventing a zero there would poison every rule downstream.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Features {
    /// Stable id, e.g. `capture_042`. Becomes the fact subject.
    pub capture_id: String,
    pub sample_rate: f64,
    pub sample_count: usize,

    pub rms: Option<f64>,
    pub peak_to_peak: Option<f64>,
    pub dc_offset: Option<f64>,
    /// Fundamental estimated locally from mean crossings — the reading a
    /// scope's frequency counter gives, without a gateway call.
    pub frequency_hz: Option<f64>,
    pub period_ms: Option<f64>,
    /// Fraction of each cycle spent above the mean, as a percentage.
    pub duty_pct: Option<f64>,
    pub dominant_hz: Option<f64>,
    pub thd_pct: Option<f64>,
    /// Regime label from `linalg.cluster`, if the chain produced one.
    pub regime: Option<String>,
    /// The capture this one follows, for drift rules.
    pub previous: Option<String>,
}

impl Features {
    /// Measurements computable locally, with no gateway round trip.
    ///
    /// RMS, pk-pk and DC are cheap and wanted every frame for the meter, so
    /// they are never worth a network call. Anything spectral comes from the
    /// chain instead — see [`Features::with_spectral`].
    pub fn from_samples(capture_id: impl Into<String>, samples: &[f32], sample_rate: f64) -> Self {
        let mut f = Self {
            capture_id: capture_id.into(),
            sample_rate,
            sample_count: samples.len(),
            ..Default::default()
        };
        if samples.is_empty() {
            return f;
        }

        let n = samples.len() as f64;
        let sum: f64 = samples.iter().map(|s| *s as f64).sum();
        let mean = sum / n;
        let sum_sq: f64 = samples.iter().map(|s| (*s as f64).powi(2)).sum();

        let (min, max) = samples
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(*s), hi.max(*s)));

        f.dc_offset = Some(mean);
        f.rms = Some((sum_sq / n).sqrt());
        f.peak_to_peak = Some((max - min) as f64);

        if let Some((period_samples, duty)) = period_by_crossings(samples, mean as f32, max - min) {
            let period_s = period_samples / sample_rate;
            f.frequency_hz = Some(1.0 / period_s);
            f.period_ms = Some(period_s * 1000.0);
            f.duty_pct = Some(duty * 100.0);
        }
        f
    }

    /// Fold in what the chain reported.
    pub fn with_spectral(mut self, dominant_hz: Option<f64>, thd_pct: Option<f64>) -> Self {
        self.dominant_hz = dominant_hz;
        self.thd_pct = thd_pct;
        self
    }

    pub fn with_regime(mut self, regime: impl Into<String>) -> Self {
        self.regime = Some(regime.into());
        self
    }

    pub fn following(mut self, previous: impl Into<String>) -> Self {
        self.previous = Some(previous.into());
        self
    }

    /// Render as `logic.assert` fact objects.
    ///
    /// Uses DG's typed-triple vocabulary: `metric` for numbers, `attribute` for
    /// strings, `relation` for links. Absent measurements emit no fact at all —
    /// a missing fact is honest, a zero is a lie a rule will act on.
    pub fn to_facts(&self) -> Vec<Value> {
        let id = &self.capture_id;
        let mut facts = vec![
            json!({"type": "entity", "name": id}),
            json!({"type": "tag", "entity": id, "tag": "capture"}),
            json!({"type": "metric", "entity": id, "metric": "sample_rate_hz", "value": self.sample_rate}),
            json!({"type": "metric", "entity": id, "metric": "sample_count", "value": self.sample_count}),
        ];

        let metrics: [(&str, Option<f64>); 8] = [
            ("rms", self.rms),
            ("peak_to_peak", self.peak_to_peak),
            ("dc_offset", self.dc_offset),
            ("frequency_hz", self.frequency_hz),
            ("period_ms", self.period_ms),
            ("duty_pct", self.duty_pct),
            ("dominant_hz", self.dominant_hz),
            ("thd_pct", self.thd_pct),
        ];
        for (name, value) in metrics {
            if let Some(v) = value {
                facts.push(
                    json!({"type": "metric", "entity": id, "metric": name, "value": round6(v)}),
                );
            }
        }

        if let Some(regime) = &self.regime {
            facts.push(
                json!({"type": "attribute", "entity": id, "attribute": "regime", "value": regime}),
            );
        }
        if let Some(prev) = &self.previous {
            facts.push(
                json!({"type": "relation", "subject": id, "relation": "follows", "object": prev}),
            );
        }

        facts
    }

    /// A `logic.batch` payload: assert every fact in one charged round trip.
    ///
    /// `logic.batch` exists as "the credit-economics fix for real-time loops" —
    /// exactly this problem. Per-frame asserts as separate calls would charge
    /// once per fact.
    pub fn to_batch_args(&self, namespace: &str) -> Value {
        json!({
            "namespace": namespace,
            "ops": [{ "op": "assert", "facts": self.to_facts() }],
        })
    }
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// Period in samples and duty cycle from rising crossings of `level`, with
/// hysteresis of 5% of `span` so noise riding on the signal does not count
/// as extra cycles.
///
/// Needs at least two rising crossings; a frame holding less than one full
/// cycle has no period to report, and reports none. Duty is the fraction of
/// samples above the level between the first and last crossing, so it is
/// measured over whole cycles only.
fn period_by_crossings(samples: &[f32], level: f32, span: f32) -> Option<(f64, f64)> {
    if span <= 0.0 {
        return None;
    }
    let hyst = span * 0.05;
    let mut armed = false;
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    let mut crossings = 0usize;
    for (i, s) in samples.iter().enumerate() {
        if *s < level - hyst {
            armed = true;
        } else if armed && *s >= level + hyst {
            armed = false;
            crossings += 1;
            if first.is_none() {
                first = Some(i);
            }
            last = Some(i);
        }
    }
    let (first, last) = (first?, last?);
    if crossings < 2 || last <= first {
        return None;
    }
    let period = (last - first) as f64 / (crossings - 1) as f64;
    let above = samples[first..last].iter().filter(|s| **s >= level).count();
    let duty = above as f64 / (last - first) as f64;
    Some((period, duty))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_local_measurements() {
        let f = Features::from_samples("c1", &[1.0, -1.0, 1.0, -1.0], 48000.0);
        assert_eq!(f.rms, Some(1.0));
        assert_eq!(f.peak_to_peak, Some(2.0));
        assert_eq!(f.dc_offset, Some(0.0));
    }

    #[test]
    fn frequency_and_duty_come_from_mean_crossings() {
        // 1 kHz square at 48 kHz, 25% duty: 12 samples high, 36 low.
        let mut samples = Vec::new();
        for _ in 0..20 {
            samples.extend(std::iter::repeat_n(1.0f32, 12));
            samples.extend(std::iter::repeat_n(-1.0f32, 36));
        }
        let f = Features::from_samples("sq", &samples, 48_000.0);
        let freq = f.frequency_hz.unwrap();
        assert!((freq - 1000.0).abs() < 1.0, "frequency {freq}");
        assert!((f.period_ms.unwrap() - 1.0).abs() < 0.01);
        assert!(
            (f.duty_pct.unwrap() - 25.0).abs() < 1.0,
            "duty {:?}",
            f.duty_pct
        );
    }

    #[test]
    fn less_than_one_cycle_reports_no_frequency() {
        // Half a sine: one rising crossing at most, so no period.
        let samples: Vec<f32> = (0..100)
            .map(|i| (std::f32::consts::PI * i as f32 / 100.0).sin())
            .collect();
        let f = Features::from_samples("half", &samples, 48_000.0);
        assert!(f.frequency_hz.is_none(), "a guess is worse than a dash");
    }

    #[test]
    fn empty_capture_yields_no_measurements() {
        let f = Features::from_samples("c1", &[], 48000.0);
        assert!(f.rms.is_none() && f.peak_to_peak.is_none());
    }

    #[test]
    fn absent_measurements_emit_no_facts() {
        let f = Features::from_samples("c1", &[0.5, -0.5], 48000.0);
        let facts = f.to_facts();
        // No chain ran, so nothing spectral should appear.
        let names: Vec<String> = facts
            .iter()
            .filter_map(|f| f.get("metric")?.as_str().map(str::to_string))
            .collect();
        assert!(names.contains(&"rms".to_string()));
        assert!(
            !names.contains(&"dominant_hz".to_string()),
            "a missing measurement must not become a zero"
        );
    }

    #[test]
    fn spectral_and_regime_fold_in() {
        let f = Features::from_samples("c2", &[1.0, -1.0], 48000.0)
            .with_spectral(Some(440.25), Some(1.8))
            .with_regime("steady")
            .following("c1");

        let facts = f.to_facts();
        let json = serde_json::to_string(&facts).unwrap();
        assert!(json.contains("dominant_hz"));
        assert!(json.contains("\"regime\""));
        assert!(json.contains("\"follows\""));
    }

    #[test]
    fn batch_args_carry_one_op() {
        let f = Features::from_samples("c1", &[1.0], 48000.0);
        let args = f.to_batch_args("bench");
        assert_eq!(args["namespace"], json!("bench"));
        assert_eq!(args["ops"].as_array().unwrap().len(), 1);
    }
}
