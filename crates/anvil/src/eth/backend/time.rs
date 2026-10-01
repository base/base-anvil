//! Manages the block time

use crate::eth::error::BlockchainError;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::{sync::Arc, time::Duration};

// Bound development time jumps to year 9999. Mining may continue past this bound,
// but RPC-controlled offsets must not exhaust u64 or chrono's printable date range.
const MAX_BEACON_TIMESTAMP: u64 = 253_402_300_799;

/// Returns the `Utc` datetime for the given seconds since unix epoch
pub fn utc_from_secs(secs: u64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs as i64, 0).unwrap()
}

/// Manages block time
#[derive(Clone, Debug)]
pub struct TimeManager {
    /// tracks the overall applied timestamp offset
    offset: Arc<RwLock<i128>>,
    /// The timestamp of the last block header
    last_timestamp: Arc<RwLock<u64>>,
    /// Contains the next timestamp to use
    /// if this is set then the next time `[TimeManager::current_timestamp()]` is called this value
    /// will be taken and returned. After which the `offset` will be updated accordingly
    next_exact_timestamp: Arc<RwLock<Option<u64>>>,
    /// The interval to use when determining the next block's timestamp
    interval: Arc<RwLock<Option<u64>>>,
    /// If set, block timestamps are restricted to this Ethereum Beacon slot grid
    beacon_slots: Arc<RwLock<Option<BeaconSlots>>>,
}

impl TimeManager {
    pub fn new(start_timestamp: u64) -> Self {
        let time_manager = Self {
            last_timestamp: Default::default(),
            offset: Default::default(),
            next_exact_timestamp: Default::default(),
            interval: Default::default(),
            beacon_slots: Default::default(),
        };
        time_manager.reset(start_timestamp);
        time_manager
    }

    /// Resets the current time manager to the given timestamp, resetting the offsets and
    /// next block timestamp option
    pub fn reset(&self, start_timestamp: u64) {
        let current = duration_since_unix_epoch().as_secs() as i128;
        *self.last_timestamp.write() = start_timestamp;
        *self.offset.write() = (start_timestamp as i128) - current;
        self.next_exact_timestamp.write().take();
    }

    /// Restricts block timestamps to the Ethereum Beacon slot grid
    /// `genesis_time + k * seconds_per_slot`, given as `(genesis_time, seconds_per_slot)`, or
    /// lifts the restriction with `None`.
    ///
    /// Pending settings that are invalid on the grid are dropped: an interval that is not a
    /// positive multiple of the slot duration, and a next block timestamp that is not a slot after
    /// the previous block's timestamp.
    pub fn set_beacon_slots(&self, slots: Option<(u64, u64)>) -> Result<(), BlockchainError> {
        let slots = slots
            .map(|(genesis_time, seconds_per_slot)| BeaconSlots { genesis_time, seconds_per_slot });
        if let Some(slots) = slots {
            if slots.seconds_per_slot == 0
                || slots.genesis_time > MAX_BEACON_TIMESTAMP
                || slots.seconds_per_slot > MAX_BEACON_TIMESTAMP
                || *self.last_timestamp.read() > MAX_BEACON_TIMESTAMP
            {
                return Err(BlockchainError::TimestampError(
                    "Beacon schedule must have a positive duration and fit the supported date range".to_string(),
                ));
            }
            let last_timestamp = *self.last_timestamp.read();
            self.interval.write().take_if(|interval| !slots.is_interval(*interval));
            self.next_exact_timestamp
                .write()
                .take_if(|next| *next <= last_timestamp || !slots.contains(*next));
        }
        trace!(target: "time", "set beacon slots {:?}", slots);
        *self.beacon_slots.write() = slots;
        Ok(())
    }

    /// Sets the clock to `timestamp`.
    ///
    /// Without Beacon slots this is [`Self::reset`]. With Beacon slots `timestamp` must be a slot
    /// at or after the previous block's timestamp, and only the clock moves: the previous block's
    /// timestamp is canonical chain state and is never lowered. Like `reset`, this clears the next
    /// block timestamp.
    pub fn set_time(&self, timestamp: u64) -> Result<(), BlockchainError> {
        let slots = *self.beacon_slots.read();
        let Some(slots) = slots else {
            self.reset(timestamp);
            return Ok(());
        };
        let last_timestamp = *self.last_timestamp.read();
        if timestamp < last_timestamp
            || timestamp > MAX_BEACON_TIMESTAMP
            || !slots.contains(timestamp)
        {
            return Err(BlockchainError::TimestampError(format!(
                "{timestamp} is not a Beacon slot at or after previous block's timestamp {last_timestamp}"
            )));
        }
        let current = duration_since_unix_epoch().as_secs() as i128;
        *self.offset.write() = (timestamp as i128) - current;
        self.next_exact_timestamp.write().take();
        Ok(())
    }

    pub fn offset(&self) -> i128 {
        *self.offset.read()
    }

    /// Adds the given `offset` to the already tracked offset and returns the result
    fn add_offset(&self, offset: i128) -> i128 {
        let mut current = self.offset.write();
        let next = current.saturating_add(offset);
        trace!(target: "time", "adding timestamp offset={}, total={}", offset, next);
        *current = next;
        next
    }

    /// Jumps forward in time by the given seconds
    ///
    /// This will apply a permanent offset to the natural UNIX Epoch timestamp
    pub fn increase_time(&self, seconds: u64) -> i128 {
        self.add_offset(seconds as i128)
    }

    /// Sets the exact timestamp to use in the next block
    /// Fails if it's before the last timestamp, or with Beacon slots, if it's not a slot after the
    /// last timestamp
    pub fn set_next_block_timestamp(&self, timestamp: u64) -> Result<(), BlockchainError> {
        trace!(target: "time", "override next timestamp {}", timestamp);
        let last_timestamp = *self.last_timestamp.read();
        if let Some(slots) = *self.beacon_slots.read() {
            if timestamp <= last_timestamp
                || timestamp > MAX_BEACON_TIMESTAMP
                || !slots.contains(timestamp)
            {
                return Err(BlockchainError::TimestampError(format!(
                    "{timestamp} is not a Beacon slot after previous block's timestamp {last_timestamp}"
                )));
            }
        } else if timestamp < last_timestamp {
            return Err(BlockchainError::TimestampError(format!(
                "{timestamp} is lower than previous block's timestamp"
            )));
        }
        self.next_exact_timestamp.write().replace(timestamp);
        Ok(())
    }

    /// Sets an interval to use when computing the next timestamp
    ///
    /// If an interval already exists, this will update the interval, otherwise a new interval will
    /// be set starting with the current timestamp. With Beacon slots the interval must be a
    /// positive multiple of the slot duration.
    pub fn set_block_timestamp_interval(&self, interval: u64) -> Result<(), BlockchainError> {
        trace!(target: "time", "set interval {}", interval);
        if let Some(slots) = *self.beacon_slots.read()
            && !slots.is_interval(interval)
        {
            return Err(BlockchainError::TimestampError(format!(
                "interval {interval} is not a positive multiple of the {}s Beacon slot duration",
                slots.seconds_per_slot
            )));
        }
        self.interval.write().replace(interval);
        Ok(())
    }

    /// Removes the interval if it exists
    pub fn remove_block_timestamp_interval(&self) -> bool {
        if self.interval.write().take().is_some() {
            trace!(target: "time", "removed interval");
            true
        } else {
            false
        }
    }

    /// Computes the next timestamp without updating internals
    fn compute_next_timestamp(&self) -> (u64, Option<i128>) {
        let current = duration_since_unix_epoch().as_secs() as i128;
        let last_timestamp = *self.last_timestamp.read();
        let slots = *self.beacon_slots.read();

        let (mut next_timestamp, update_offset) = if let Some(next) =
            *self.next_exact_timestamp.read()
        {
            (next, true)
        } else if let Some(interval) = *self.interval.read() {
            (last_timestamp.saturating_add(interval), false)
        } else {
            let clock = current.saturating_add(self.offset());
            // Clamp instead of wrapping so slot rounding sees the intended clock.
            let clock =
                if slots.is_some() { clock.clamp(0, MAX_BEACON_TIMESTAMP as i128) } else { clock };
            (clock as u64, false)
        };
        if let Some(slots) = slots {
            next_timestamp =
                slots.next_after(last_timestamp, next_timestamp.min(MAX_BEACON_TIMESTAMP));
        } else if next_timestamp < last_timestamp {
            // Ensures that the timestamp is always increasing
            next_timestamp = last_timestamp + 1;
        }
        let next_offset = update_offset.then_some((next_timestamp as i128) - current);
        (next_timestamp, next_offset)
    }

    /// Returns the current timestamp and updates the underlying offset and interval accordingly
    pub fn next_timestamp(&self) -> u64 {
        let (next_timestamp, next_offset) = self.compute_next_timestamp();
        // Make sure we reset the `next_exact_timestamp`
        self.next_exact_timestamp.write().take();
        if let Some(next_offset) = next_offset {
            *self.offset.write() = next_offset;
        }
        *self.last_timestamp.write() = next_timestamp;
        next_timestamp
    }

    /// Returns the current timestamp for a call that does _not_ update the value
    pub fn current_call_timestamp(&self) -> u64 {
        let (next_timestamp, _) = self.compute_next_timestamp();
        next_timestamp
    }
}

/// Ethereum Beacon slot grid: slot timestamps are `genesis_time + k * seconds_per_slot`
#[derive(Clone, Copy, Debug)]
struct BeaconSlots {
    genesis_time: u64,
    /// Always positive
    seconds_per_slot: u64,
}

impl BeaconSlots {
    /// Returns true if `timestamp` is a slot timestamp
    fn contains(&self, timestamp: u64) -> bool {
        timestamp
            .checked_sub(self.genesis_time)
            .is_some_and(|elapsed| elapsed.is_multiple_of(self.seconds_per_slot))
    }

    /// Returns true if `interval` advances from one slot to a later one
    fn is_interval(&self, interval: u64) -> bool {
        interval != 0
            && interval <= MAX_BEACON_TIMESTAMP
            && interval.is_multiple_of(self.seconds_per_slot)
    }

    /// Uses the slot containing the clock, or the next slot if the parent already occupies it.
    /// Inputs are bounded before reaching here; normal mining cannot exhaust the integer range.
    fn next_after(&self, last: u64, candidate: u64) -> u64 {
        let clock_slot = candidate.saturating_sub(self.genesis_time) / self.seconds_per_slot;
        let next_slot =
            (last + 1).saturating_sub(self.genesis_time).div_ceil(self.seconds_per_slot);
        self.genesis_time + clock_slot.max(next_slot) * self.seconds_per_slot
    }
}

/// Returns the current duration since unix epoch.
pub fn duration_since_unix_epoch() -> Duration {
    use std::time::SystemTime;
    let now = SystemTime::now();
    now.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|err| panic!("Current time {now:?} is invalid: {err:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENESIS: u64 = 1000;
    const SLOT: u64 = 12;
    const TIP: u64 = 1240;

    fn beacon(tip: u64) -> TimeManager {
        let time = TimeManager::new(tip);
        time.set_beacon_slots(Some((GENESIS, SLOT))).unwrap();
        time
    }

    #[test]
    fn beacon_implicit_timestamp_is_next_slot() {
        let time = beacon(TIP);
        assert_eq!(time.current_call_timestamp(), 1252);
        assert_eq!(time.next_timestamp(), 1252);
        assert_eq!(time.next_timestamp(), 1264);
    }

    #[test]
    fn beacon_next_block_timestamp_must_be_later_slot() {
        let time = beacon(TIP);
        assert!(time.set_next_block_timestamp(1253).is_err());
        assert!(time.set_next_block_timestamp(TIP).is_err());
        assert!(time.set_next_block_timestamp(1228).is_err());
        time.set_next_block_timestamp(1276).unwrap();
        assert_eq!(time.current_call_timestamp(), 1276);
        assert_eq!(time.next_timestamp(), 1276);
    }

    #[test]
    fn beacon_interval_must_be_positive_slot_multiple() {
        let time = beacon(TIP);
        assert!(time.set_block_timestamp_interval(5).is_err());
        assert!(time.set_block_timestamp_interval(0).is_err());
        time.set_block_timestamp_interval(24).unwrap();
        assert_eq!(time.current_call_timestamp(), 1264);
        assert_eq!(time.next_timestamp(), 1264);
        assert_eq!(time.next_timestamp(), 1288);
    }

    #[test]
    fn beacon_set_time_cannot_move_behind_tip() {
        let time = beacon(TIP);
        time.set_next_block_timestamp(1300).unwrap();
        assert_eq!(time.next_timestamp(), 1300);

        assert!(time.set_time(1288).is_err());
        assert!(time.set_time(1301).is_err());
        time.set_time(1300).unwrap();
        // The mined tip stays canonical, so the next block lands in the following slot.
        assert!(time.set_next_block_timestamp(1300).is_err());
        assert_eq!(time.current_call_timestamp(), 1312);
        assert_eq!(time.next_timestamp(), 1312);
    }

    #[test]
    fn beacon_reset_clears_one_shot_and_keeps_interval() {
        let time = beacon(TIP);
        time.set_block_timestamp_interval(24).unwrap();
        time.set_next_block_timestamp(1300).unwrap();
        time.reset(TIP);
        assert_eq!(time.next_timestamp(), 1264);
    }

    #[test]
    fn beacon_activation_drops_incompatible_pending_state() {
        let time = TimeManager::new(TIP);
        time.set_block_timestamp_interval(30).unwrap();
        time.set_next_block_timestamp(1253).unwrap();
        time.set_beacon_slots(Some((GENESIS, SLOT))).unwrap();
        assert_eq!(time.next_timestamp(), 1252);
        assert!(time.set_beacon_slots(Some((GENESIS, 0))).is_err());
    }

    #[test]
    fn beacon_extreme_time_jump_leaves_room_to_continue_mining() {
        let time = beacon(TIP);
        time.increase_time(u64::MAX);
        let first = time.next_timestamp();
        assert!(first <= MAX_BEACON_TIMESTAMP);
        assert_eq!((first - GENESIS) % SLOT, 0);
        assert_eq!(time.next_timestamp(), first + SLOT);
        assert!(time.set_next_block_timestamp(u64::MAX).is_err());
        assert!(time.set_time(u64::MAX).is_err());
        assert!(time.set_block_timestamp_interval(u64::MAX / SLOT * SLOT).is_err());
    }

    #[test]
    fn beacon_clock_uses_containing_slot_after_elapsed_seconds() {
        let time = beacon(TIP);
        time.set_time(1300).unwrap();
        time.increase_time(1);
        assert_eq!(time.next_timestamp(), 1300);
        let time = beacon(TIP);
        time.increase_time(5);
        time.increase_time(12);
        assert_eq!(time.next_timestamp(), 1252);
    }

    #[test]
    fn legacy_behavior_is_unchanged() {
        let time = TimeManager::new(TIP);
        time.set_next_block_timestamp(TIP).unwrap();
        time.set_next_block_timestamp(1253).unwrap();
        assert_eq!(time.next_timestamp(), 1253);
        time.set_block_timestamp_interval(0).unwrap();
        time.set_block_timestamp_interval(5).unwrap();
        assert_eq!(time.next_timestamp(), 1258);
        time.set_time(100).unwrap();
        assert_eq!(time.next_timestamp(), 105);
    }

    #[test]
    fn clearing_beacon_slots_restores_legacy_behavior() {
        let time = beacon(TIP);
        time.set_beacon_slots(None).unwrap();
        time.set_next_block_timestamp(1253).unwrap();
        assert_eq!(time.next_timestamp(), 1253);
        time.set_block_timestamp_interval(5).unwrap();
        time.set_time(100).unwrap();
        assert_eq!(time.next_timestamp(), 105);
    }
}
