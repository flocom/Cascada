//! Maps master tickets to the slave ticket(s) mirroring them, so Close and
//! Modify commands reach the correct slave order.

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

const PENDING_TTL: Duration = Duration::from_secs(60);
/// Run the pending-GC sweep every N `mark_pending` calls instead of every call.
/// Amortises the O(n) retain on a hot master pumping trades.
const PENDING_GC_EVERY: u32 = 50;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct MasterKey {
    pub account_id: String,
    pub ticket: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlaveRef {
    pub account_id: String,
    pub ticket: String,
}

#[derive(Default)]
pub struct TicketMap {
    by_master: DashMap<MasterKey, Vec<SlaveRef>>,
    pending: DashMap<(String, String), (MasterKey, Instant)>,
    gc_tick: AtomicU32,
}

impl TicketMap {
    pub fn new() -> Self { Self::default() }

    pub fn mark_pending(&self, slave_account: &str, origin_ticket: &str, master: MasterKey) {
        // Amortised GC: only sweep once every PENDING_GC_EVERY inserts.
        if self.gc_tick.fetch_add(1, Ordering::Relaxed) % PENDING_GC_EVERY == 0 {
            self.gc_pending();
        }
        self.pending.insert(
            (slave_account.to_string(), origin_ticket.to_string()),
            (master, Instant::now()),
        );
    }

    fn gc_pending(&self) {
        let now = Instant::now();
        self.pending.retain(|_, (_, ts)| now.duration_since(*ts) < PENDING_TTL);
    }

    pub fn resolve_slave_open(
        &self,
        slave_account: &str,
        origin_ticket: &str,
        slave_ticket: &str,
    ) -> bool {
        let key = (slave_account.to_string(), origin_ticket.to_string());
        if let Some((_, (master, _))) = self.pending.remove(&key) {
            self.by_master
                .entry(master)
                .or_default()
                .push(SlaveRef {
                    account_id: slave_account.to_string(),
                    ticket: slave_ticket.to_string(),
                });
            true
        } else {
            false
        }
    }

    pub fn slaves_for(&self, master: &MasterKey) -> Vec<SlaveRef> {
        self.by_master.get(master).map(|v| v.clone()).unwrap_or_default()
    }

    pub fn drop_master(&self, master: &MasterKey) {
        self.by_master.remove(master);
    }

    pub fn clear(&self) {
        self.by_master.clear();
        self.pending.clear();
    }
}
