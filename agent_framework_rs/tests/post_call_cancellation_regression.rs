use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::thread;

use agentkit::events::{DONE, TOOL_CALL, TOOL_RESULT};
use agentkit::llm::Chunk;
use agentkit::testing::FakeLlm;
use agentkit::{new_cancel, Agent, EventBus, EventData, ToolEffect, ToolRegistry};
use serde_json::{json, Value};

#[test]
fn post_cancel_ok_from_destructive_tool_is_reported_as_aborted() {
    let (started_tx, started_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let returned_ok = Arc::new(AtomicUsize::new(0));
    let returned_ok_in_tool = Arc::clone(&returned_ok);

    let mut tools = ToolRegistry::new();
    tools.add("destroy", "blocking destructive tool", json!({"type":"object"}), move |_args: Value| {
        started_tx.send(()).expect("test must wait for tool start");
        release_rx
            .lock()
            .expect("release receiver lock must not be poisoned")
            .recv()
            .expect("test must release tool");
        returned_ok_in_tool.fetch_add(1, Ordering::SeqCst);
        Ok("destructive operation completed".to_owned())
    });
    tools.declare("destroy", ToolEffect::Destructive);

    let llm = Arc::new(FakeLlm::new(vec![vec![Chunk::tool(
        0, "destroy-call", "destroy", "{}",
    )]]));
    let mut agent = Agent::builder(llm).tools(tools).build();
    let cancel = new_cancel();
    let bus = EventBus::new();
    let events = bus.subscribe();
    let worker_cancel = Arc::clone(&cancel);
    let worker_bus = bus.clone();

    let worker = thread::spawn(move || {
        agent.run_on_bus("destroy", &worker_bus, 7, Some(&worker_cancel), "cancel-regression")
    });

    started_rx.recv().expect("tool must start before cancellation");
    cancel.store(true, Ordering::SeqCst);
    release_tx.send(()).expect("tool must be released after cancellation");
    let _ = worker.join().expect("agent worker must not panic");

    let mut seen = Vec::new();
    while let Ok(event) = events.try_recv() {
        seen.push(event);
    }

    assert_eq!(returned_ok.load(Ordering::SeqCst), 1);
    assert!(seen.iter().any(|event| event.etype == TOOL_CALL && event.call_id == "destroy-call"));
    assert!(seen.iter().any(|event| {
        event.etype == TOOL_RESULT
            && event.call_id == "destroy-call"
            && matches!(
                &event.data,
                EventData::ToolResult { name, result }
                    if name == "destroy"
                        && result == "ERROR: abgebrochen — nicht mehr ausgeführt."
            )
    }));
    assert!(seen.iter().any(|event| event.etype == DONE));
}
