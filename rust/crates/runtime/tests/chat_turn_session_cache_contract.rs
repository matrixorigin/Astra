use std::{fs, path::PathBuf};

use mo_agent_runtime::SessionCache;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Deserialize)]
struct WriteOp {
    time: f64,
    key: String,
    value: Map<String, Value>,
}

#[derive(Deserialize)]
struct ReadOp {
    time: f64,
    key: String,
    expected: Option<Map<String, Value>>,
}

#[derive(Deserialize)]
struct CacheCase {
    maxsize: usize,
    ttl: f64,
    writes: Vec<WriteOp>,
    reads: Vec<ReadOp>,
}

#[derive(Deserialize)]
struct SessionCacheContract {
    evicts_history_and_tools_together: CacheCase,
    ttl_expiry: CacheCase,
    access_refreshes_ttl: CacheCase,
}

fn load_contract() -> SessionCacheContract {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("fixtures/contracts/chat_turn_session_cache_contract.json");
    let content =
        fs::read_to_string(path).expect("chat turn session cache contract fixture should exist");
    serde_json::from_str(&content)
        .expect("chat turn session cache contract fixture should be valid JSON")
}

#[test]
fn evicts_history_and_tools_together_matches_shared_contract() {
    assert_cache_case(load_contract().evicts_history_and_tools_together);
}

#[test]
fn ttl_expiry_matches_shared_contract() {
    assert_cache_case(load_contract().ttl_expiry);
}

#[test]
fn access_refreshes_ttl_matches_shared_contract() {
    assert_cache_case(load_contract().access_refreshes_ttl);
}

fn assert_cache_case(case: CacheCase) {
    let mut cache = SessionCache::new(case.maxsize, case.ttl);
    for write in case.writes {
        cache.insert(write.key, write.value, write.time);
    }

    for read in case.reads {
        let actual = cache.get(&read.key, read.time).map(strip_ts);
        let expected = read.expected.map(strip_ts);
        assert_eq!(actual, expected);
    }
}

fn strip_ts(mut entry: Map<String, Value>) -> Map<String, Value> {
    entry.remove("ts");
    entry
}
