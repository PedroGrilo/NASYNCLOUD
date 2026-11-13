use std::collections::{hash_map, HashMap};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// How often expired transactions are dropped (NASYNCLOUD patch, see below).
const HOUSEKEEPING_EVERY: Duration = Duration::from_secs(1);

/// `TransactionTracker` tracks the state of transactions to detect retransmissions.
pub struct TransactionTracker {
    retention_period: Duration,
    transactions: Mutex<Transactions>,
}

struct Transactions {
    map: HashMap<(u32, String), TransactionState>,
    last_housekeeping: SystemTime,
}

impl TransactionTracker {
    pub fn new(retention_period: Duration) -> Self {
        Self {
            retention_period,
            transactions: Mutex::new(Transactions { map: HashMap::new(), last_housekeeping: SystemTime::UNIX_EPOCH }),
        }
    }

    /// Checks if the transaction is a retransmission.
    /// If it's a new transaction, it is marked as `InProgress`.
    ///
    /// Returns `true` if the transaction is a retransmission, `false` otherwise.
    pub fn is_retransmission(&self, xid: u32, client_addr: &str) -> bool {
        let key = (xid, client_addr.to_string());
        let mut transactions = self.transactions.lock().expect("unable to unlock transactions mutex");
        // NASYNCLOUD patch: upstream scans every transaction of the retention
        // period (60 s) on every request, under this lock. At a thousand
        // requests per second (copying many small files) that is tens of
        // thousands of entries per request, and each request gets slower the
        // faster the client goes. Once a second is plenty.
        let now = SystemTime::now();
        if now.duration_since(transactions.last_housekeeping).map_or(true, |d| d >= HOUSEKEEPING_EVERY) {
            housekeeping(&mut transactions.map, now, self.retention_period);
            transactions.last_housekeeping = now;
        }
        if let hash_map::Entry::Vacant(e) = transactions.map.entry(key) {
            e.insert(TransactionState::InProgress);
            false
        } else {
            true
        }
    }

    /// Marks the transaction as processed.
    pub fn mark_processed(&self, xid: u32, client_addr: &str) {
        let key = (xid, client_addr.to_string());
        let completion_time = SystemTime::now();
        let mut transactions = self.transactions.lock().expect("unable to unlock transactions mutex");
        if let Some(tx) = transactions.map.get_mut(&key) {
            *tx = TransactionState::Completed(completion_time);
        }
    }
}

fn housekeeping(transactions: &mut HashMap<(u32, String), TransactionState>, now: SystemTime, max_age: Duration) {
    let cutoff = now - max_age;
    transactions.retain(|_, v| match v {
        TransactionState::InProgress => true,
        TransactionState::Completed(completion_time) => *completion_time >= cutoff,
    });
}

pub enum TransactionState {
    InProgress,
    Completed(SystemTime),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_retransmissions_and_forgets_old_ones() {
        let t = TransactionTracker::new(Duration::from_millis(50));
        assert!(!t.is_retransmission(1, "a"));
        assert!(t.is_retransmission(1, "a"));
        assert!(!t.is_retransmission(1, "b"));
        t.mark_processed(1, "a");
        assert!(t.is_retransmission(1, "a"));
        std::thread::sleep(HOUSEKEEPING_EVERY + Duration::from_millis(100));
        assert!(!t.is_retransmission(1, "a"), "expired transaction still remembered");
        // Still in progress: never forgotten.
        assert!(t.is_retransmission(1, "b"));
    }
}
