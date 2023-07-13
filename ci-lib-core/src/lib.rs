use std::time::{SystemTime, UNIX_EPOCH};


pub mod protocol;
pub mod sql;
pub mod dbctx;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("now is later than epoch")
        .as_millis() as u64
}
