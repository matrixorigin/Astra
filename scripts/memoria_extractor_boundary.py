#!/usr/bin/env python3
"""Run Astra windows through the exact supplied Memoria LLM extraction method.

Uses already-built serde libraries and a controlled, immediately-ready LLM.
No service credentials, database, live model or network are used. This checks
the extraction input boundary, not the complete Memoria API or extraction quality.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile


HARNESS = r'''
use std::{cell::RefCell, future::Future, task::{Context, Poll, Waker}};
use serde_json::{Value, json};
#[derive(Debug)]
enum MemoriaError { Internal(String) }
struct ChatMessage { role: String, content: String }
struct LlmClient { inputs: RefCell<Vec<String>> }
impl LlmClient {
    async fn chat(&self, messages: &[ChatMessage], _: f64, _: Option<u32>) -> Result<String, &'static str> {
        assert_eq!(messages[1].role, "user");
        self.inputs.borrow_mut().push(messages[1].content.clone());
        Ok("[]".into())
    }
}
const OBSERVER_EXTRACTION_PROMPT: &str = "controlled extraction fixture";
const OBSERVER_EXCLUSION_PROMPT: &str = "controlled exclusion fixture";
fn parse_json_array(value: &str) -> Result<Vec<Value>, MemoriaError> {
    serde_json::from_str(value).map_err(|e| MemoriaError::Internal(e.to_string()))
}
struct MemoryService;
fn ready<F: Future>(future: F) -> F::Output {
    match std::pin::pin!(future).as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the controlled LLM must not perform asynchronous I/O"),
    }
}
#[test]
fn original_unwindowed_backlog_reproduces_twenty_of_thirty() {
    let llm = LlmClient { inputs: RefCell::default() };
    let messages = (0..30).map(|i| json!({"role":"user","content":format!("FACT-{i:02}-END")})).collect::<Vec<_>>();
    ready(MemoryService.extract_via_llm(&llm, &messages, &[])).unwrap();
    let inputs = llm.inputs.borrow();
    assert_eq!(messages.iter().filter(|m| inputs[0].contains(m["content"].as_str().unwrap())).count(), 20);
}
#[test]
fn actual_memoria_extractor_sees_every_windowed_fragment_without_trimming() {
    let llm = LlmClient { inputs: RefCell::default() };
    let messages = (0..30).map(|i| json!({"role":"user","content":
        format!("FACT-{i:02}-{}-END", (0..200).map(|j| format!("字{j:03}🙂")).collect::<String>())
    })).collect::<Vec<_>>();
    let input = window::ObservationInput::new(&[], &messages);
    let mut cursor = window::ObservationTextCursor::default();
    loop {
        let batch = input.window(cursor).unwrap();
        let wire = batch.messages.into_iter().map(Value::Object).collect::<Vec<_>>();
        ready(MemoryService.extract_via_llm(&llm, &wire, &[])).unwrap();
        let expected = wire.iter().map(|m| format!("[{}]: {}\n", m["role"].as_str().unwrap(), m["content"].as_str().unwrap())).collect::<String>();
        assert_eq!(llm.inputs.borrow().last().unwrap(), &expected);
        cursor = batch.next;
        if batch.complete { break; }
    }
    let captured = llm.inputs.borrow();
    for message in &messages {
        let chars = message["content"].as_str().unwrap().chars().collect::<Vec<_>>();
        for part in chars.chunks(500) {
            let fragment = part.iter().collect::<String>();
            assert!(captured.iter().any(|input| input.contains(&fragment)), "missing newly completed text");
        }
    }
}
'''


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--memoria-service-source", required=True, type=Path)
    parser.add_argument("--deps", type=Path, default=Path("target/debug/deps"))
    args = parser.parse_args()
    source = args.memoria_service_source.read_text()
    start = source.index("    const MAX_EXTRACT_MESSAGES:")
    end = source.index("    pub async fn user_sql_store", start)
    method = source[start:end]
    if "async fn extract_via_llm(" not in method:
        raise SystemExit("Memoria extraction method moved; inspect the current boundary")
    root = Path(__file__).resolve().parents[1]
    window = root / "crates/services/src/session_observation/window.rs"
    deps = args.deps.resolve()
    externs = []
    for crate in ("serde", "serde_json"):
        candidates = list(deps.glob(f"lib{crate}-*.rlib"))
        if not candidates:
            raise SystemExit(f"Build the Astra services tests first: missing {crate} library")
        library = max(candidates, key=lambda path: path.stat().st_mtime)
        externs.extend(["--extern", f"{crate}={library}"])
    with tempfile.TemporaryDirectory(prefix="astra-memoria-extractor-") as directory:
        directory = Path(directory)
        test = directory / "boundary.rs"
        test.write_text(
            "#![allow(dead_code)]\n#[path = "
            + json.dumps(str(window), ensure_ascii=False)
            + "] mod window;\n"
            + HARNESS
            + "\nimpl MemoryService {\n"
            + method
            + "\n}\n"
        )
        executable = directory / "boundary"
        subprocess.run(
            ["rustc", "--edition=2024", "--test", str(test), "-L", f"dependency={deps}",
             *externs, "-o", str(executable)], check=True,
        )
        print("Memoria method SHA256:", hashlib.sha256(method.encode()).hexdigest(), flush=True)
        subprocess.run([str(executable)], check=True)


if __name__ == "__main__":
    main()
