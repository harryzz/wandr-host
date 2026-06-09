use crate::bindings::my::skiko_gfx::scheduler::Host;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::time::{Duration, Instant};

/// One entry in the scheduler's min-heap. PartialOrd/Ord reversed so the
/// std BinaryHeap (a max-heap) returns the earliest deadline first.
#[derive(Eq, PartialEq)]
struct Entry {
    deadline: Instant,
    handle: u32,
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.deadline.cmp(&self.deadline)
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub struct SchedulerState {
    heap:        BinaryHeap<Entry>,
    /// handle -> callback_id
    handles:     HashMap<u32, u32>,
    cancelled:   HashSet<u32>,
    next_handle: u32,
}

impl SchedulerState {
    /// Pop every entry whose deadline has passed and return the live
    /// callback_ids in firing order. Cancelled and missing handles are
    /// silently dropped.
    pub fn drain_due(&mut self, now: Instant) -> Vec<u32> {
        let mut due = Vec::new();
        while let Some(top) = self.heap.peek() {
            if top.deadline > now { break; }
            let entry = self.heap.pop().unwrap();
            if self.cancelled.remove(&entry.handle) { continue; }
            if let Some(callback_id) = self.handles.remove(&entry.handle) {
                due.push(callback_id);
            }
        }
        due
    }
}

impl Host for crate::HostState {
    fn schedule_delayed(&mut self, delay_ms: u32, callback_id: u32) -> u32 {
        let handle = self.scheduler.next_handle;
        self.scheduler.next_handle = handle.wrapping_add(1);
        self.scheduler.handles.insert(handle, callback_id);
        self.scheduler.heap.push(Entry {
            deadline: Instant::now() + Duration::from_millis(delay_ms as u64),
            handle,
        });
        handle
    }

    fn cancel(&mut self, handle: u32) {
        // Mark for skip; the heap entry is popped lazily on drain.
        if self.scheduler.handles.remove(&handle).is_some() {
            self.scheduler.cancelled.insert(handle);
        }
    }
}
