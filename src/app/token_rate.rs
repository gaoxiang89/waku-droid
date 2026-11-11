//! Live output throughput (tokens per second) for the turn in flight.
//!
//! Providers report exact output token counts only once a message or turn
//! settles, if at all, so the live figure is estimated from the streamed
//! text and reasoning deltas. Two numbers come out of one meter:
//!
//! - the **live** rate, over a short sliding window of recent deltas, shown
//!   while the model is producing output;
//! - the turn **average**, over generation time only — a gap between deltas
//!   longer than [`BURST_GAP`] is a tool call or another pause and is not
//!   counted — persisted on the turn as [`TurnStreamStats`].
//!
//! Each delta is timestamped when the app processes it, which already follows
//! the stream commit cadence; nothing here touches I/O, and the per-frame read
//! is a scan over a handful of window samples.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::model::TurnStreamStats;

/// A longer silence between deltas ends a generation burst.
const BURST_GAP: Duration = Duration::from_millis(1_500);
/// How far back the live rate looks.
const LIVE_WINDOW: Duration = Duration::from_secs(3);
/// The live rate needs at least this much measured span; shorter spans are
/// dominated by chunk batching and would spike.
const MIN_LIVE_SPAN: Duration = Duration::from_millis(500);

/// Rough token count of streamed text. ASCII runs about four characters per
/// token across the providers' tokenizers, CJK and similar scripts about one
/// per character, and other non-ASCII text falls in between.
pub(super) fn estimate_tokens(text: &str) -> f64 {
    text.chars()
        .map(|character| {
            if character.is_ascii() {
                0.25
            } else if is_wide_script(character) {
                1.0
            } else {
                0.5
            }
        })
        .sum()
}

fn is_wide_script(character: char) -> bool {
    matches!(
        u32::from(character),
        0x1100..=0x11FF       // Hangul Jamo
            | 0x2E80..=0x9FFF // CJK radicals, kana, CJK unified ideographs
            | 0xAC00..=0xD7AF // Hangul syllables
            | 0xF900..=0xFAFF // CJK compatibility ideographs
            | 0xFF00..=0xFFEF // Half- and full-width forms
            | 0x20000..=0x3FFFF // CJK extensions
    )
}

#[derive(Debug)]
pub(super) struct TokenRateMeter {
    turn_id: Uuid,
    /// Fractional totals; the persisted stats are these rounded.
    tokens: f64,
    generation: Duration,
    last_delta_at: Option<Instant>,
    /// Deltas inside the live window, each after the first in its burst
    /// carrying the gap since the previous delta. `(arrival, tokens)`.
    window: VecDeque<(Instant, f64)>,
}

impl TokenRateMeter {
    /// A meter for `turn_id`, continuing from the stats already on the turn
    /// so a meter rebuilt mid-turn does not reset its average.
    pub(super) fn new(turn_id: Uuid, existing: Option<TurnStreamStats>) -> Self {
        let existing = existing.unwrap_or_default();
        Self {
            turn_id,
            tokens: existing.tokens as f64,
            generation: Duration::from_millis(existing.generation_ms),
            last_delta_at: None,
            window: VecDeque::new(),
        }
    }

    pub(super) fn turn_id(&self) -> Uuid {
        self.turn_id
    }

    /// Record one delta arriving at `now`, returning the updated turn stats.
    pub(super) fn record(&mut self, text: &str, now: Instant) -> TurnStreamStats {
        let tokens = estimate_tokens(text);
        let continuing = self
            .last_delta_at
            .map(|last| now.saturating_duration_since(last))
            .filter(|gap| *gap <= BURST_GAP);
        match continuing {
            Some(gap) => {
                self.generation += gap;
                self.tokens += tokens;
            }
            None => {
                // The first chunk of a burst was generated over time this
                // meter never saw; it opens the window without adding to it.
                self.window.clear();
            }
        }
        self.window
            .push_back((now, if continuing.is_some() { tokens } else { 0.0 }));
        self.last_delta_at = Some(now);
        while self
            .window
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > LIVE_WINDOW)
        {
            self.window.pop_front();
        }
        self.stats()
    }

    pub(super) fn stats(&self) -> TurnStreamStats {
        TurnStreamStats {
            tokens: self.tokens.round() as u64,
            generation_ms: self.generation.as_millis() as u64,
        }
    }

    /// The rate over the recent window while output is still arriving.
    /// Measured up to the newest delta rather than to `now`, so the figure
    /// changes only when a delta lands instead of drifting every frame.
    pub(super) fn live_rate(&self, now: Instant) -> Option<f64> {
        let last = self.last_delta_at?;
        if now.saturating_duration_since(last) > BURST_GAP {
            return None;
        }
        let (oldest, _) = *self.window.front()?;
        let span = last.saturating_duration_since(oldest);
        if span < MIN_LIVE_SPAN {
            return None;
        }
        // The oldest sample's tokens predate the measured span.
        let tokens: f64 = self.window.iter().skip(1).map(|(_, tokens)| tokens).sum();
        (tokens > 0.0).then(|| tokens / span.as_secs_f64())
    }
}

/// `~42 tok/s`, with one decimal below ten so slow models do not read as 0.
/// The tilde marks the figure as an estimate from streamed text.
pub(super) fn format_token_rate(rate: f64) -> String {
    let value = if rate < 10.0 {
        format!("{rate:.1}")
    } else {
        format!("{rate:.0}")
    };
    tr!("transcript.token_rate", rate = value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_ascii_and_cjk_differently() {
        assert_eq!(estimate_tokens("abcdefgh"), 2.0);
        assert_eq!(estimate_tokens("你好世界"), 4.0);
        assert_eq!(estimate_tokens(""), 0.0);
    }

    #[test]
    fn average_excludes_pauses_and_burst_openers() {
        let start = Instant::now();
        let mut meter = TokenRateMeter::new(Uuid::nil(), None);
        // Burst one: 4 deltas of 10 tokens, 100 ms apart.
        for step in 0..4 {
            meter.record(&"a".repeat(40), start + Duration::from_millis(step * 100));
        }
        // A long tool call, then burst two.
        let resume = start + Duration::from_secs(10);
        for step in 0..8 {
            meter.record(&"a".repeat(40), resume + Duration::from_millis(step * 100));
        }
        let stats = meter.stats();
        // 3 + 7 measured deltas over 300 + 700 ms.
        assert_eq!(stats.tokens, 100);
        assert_eq!(stats.generation_ms, 1_000);
        assert_eq!(stats.tokens_per_second(), Some(100.0));
    }

    #[test]
    fn live_rate_needs_a_measured_span_and_recent_output() {
        let start = Instant::now();
        let mut meter = TokenRateMeter::new(Uuid::nil(), None);
        meter.record(&"a".repeat(40), start);
        meter.record(&"a".repeat(40), start + Duration::from_millis(200));
        assert_eq!(meter.live_rate(start + Duration::from_millis(200)), None);
        for step in 3..=6 {
            meter.record(&"a".repeat(40), start + Duration::from_millis(step * 100));
        }
        let last = start + Duration::from_millis(600);
        // 5 measured deltas of 10 tokens over 600 ms.
        let rate = meter.live_rate(last).expect("live rate");
        assert!((rate - 50.0 / 0.6).abs() < 1e-9);
        assert_eq!(meter.live_rate(last + Duration::from_secs(2)), None);
    }

    #[test]
    fn a_rebuilt_meter_continues_the_turn_average() {
        let meter = TokenRateMeter::new(
            Uuid::nil(),
            Some(TurnStreamStats {
                tokens: 300,
                generation_ms: 6_000,
            }),
        );
        assert_eq!(meter.stats().tokens_per_second(), Some(50.0));
    }

    #[test]
    fn formats_rates_with_an_estimate_marker() {
        assert_eq!(format_token_rate(42.4), "~42 tok/s");
        assert_eq!(format_token_rate(9.54), "~9.5 tok/s");
    }
}
