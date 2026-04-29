//! Clock manipulation helper (sign-off BLK-16).
//!
//! `LiteSVM::set_sysvar::<Clock>(&Clock { unix_timestamp, .. })` is the exact
//! API verified against docs.rs/litesvm/0.10.0 (signature
//! `pub fn set_sysvar<T>(&mut self, sysvar: &T) where T: Sysvar + SysvarId +
//! SysvarSerialize`).
//!
//! We deliberately avoid `warp_to_slot` for streaming-math tests because
//! Clock-derived `unix_timestamp` from a slot count is implementation-defined
//! (slot 0 → t0, then slots advance at slot-time which LiteSVM may not
//! simulate); absolute timestamps are unambiguous.

use litesvm::LiteSVM;
use solana_program::clock::Clock;

/// Force the Clock sysvar's `unix_timestamp` to `unix_ts`. Other Clock fields
/// (slot, epoch, etc.) are filled with the current sysvar values so we don't
/// regress them — useful since some Anchor constraints may inspect slot too.
pub fn set_clock(svm: &mut LiteSVM, unix_ts: i64) {
    let mut clock: Clock = svm.get_sysvar();
    clock.unix_timestamp = unix_ts;
    svm.set_sysvar::<Clock>(&clock);
}

/// Convenience: read the current `unix_timestamp`.
pub fn now(svm: &LiteSVM) -> i64 {
    let clock: Clock = svm.get_sysvar();
    clock.unix_timestamp
}

/// Advance the clock by `delta` seconds (negative allowed — adversarial tests).
pub fn advance(svm: &mut LiteSVM, delta: i64) {
    let t = now(svm);
    set_clock(svm, t + delta);
}
