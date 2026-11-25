//! Injected time source.
//!
//! Row 10 in `docs/specsheet\04-rust-relay-decisions.md`: control holds and
//! cooldowns use a monotonic source, so an NTP step or a manual clock change
//! cannot extend or expire them. Replay arithmetic and log timestamps use wall
//! time and never go through this trait. Sources are never mixed: values are
//! only comparable inside one clock instance.

/// Milliseconds from a fixed origin, valid only for comparing values produced
/// by the same clock instance.
pub trait Clock {
    fn now_ms(&self) -> i64;
}
