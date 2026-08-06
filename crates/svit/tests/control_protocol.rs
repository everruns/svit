use std::sync::{Arc, Barrier};
use std::thread;

use svit::{
    ControlOutcome, ControlRequest, ControlResponse, Process, ProcessController, ProcessId, Value,
    value,
};

const COUNTER: &str = r#"
(define (main input)
  (let ((count (+ (read "/memory/count") (value-get input "/by"))))
    (do
      (write "/memory/count" count)
      (value-map "count" count))))
"#;

fn counter_controller(receipt_limit: usize) -> ProcessController {
    let mut process = Process::builder("svit://local/control/counter")
        .unwrap()
        .memory("count", value!(0))
        .build()
        .unwrap();
    process
        .write("/lib/counter", value!({"source": COUNTER}))
        .unwrap();
    ProcessController::with_receipt_limit(process, receipt_limit).unwrap()
}

fn request(client: &str, request: &str, version: u64, by: i64) -> ControlRequest {
    ControlRequest::activate(
        client,
        request,
        ProcessId::new("svit://local/control/counter").unwrap(),
        version,
        "counter",
        value!({"by": by}),
    )
    .unwrap()
}

#[test]
// THREAT[TM-EFF-002]
fn concurrent_clients_cannot_commit_the_same_process_version() {
    let controller = Arc::new(counter_controller(16));
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();

    for (client, request_id) in [("agent-a", "request-a"), ("agent-b", "request-b")] {
        let controller = Arc::clone(&controller);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let request = request(client, request_id, 1, 1);
            barrier.wait();
            controller.execute(request).unwrap()
        }));
    }

    barrier.wait();
    let responses = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response.outcome, ControlOutcome::Committed { .. }))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|response| matches!(response.outcome, ControlOutcome::Conflict { .. }))
            .count(),
        1
    );
    assert_eq!(controller.observe().unwrap().version, 2);
    let restored = Process::restore(&controller.snapshot().unwrap()).unwrap();
    assert_eq!(
        restored.read("/memory/count").unwrap(),
        Some(&Value::Integer(1))
    );
}

#[test]
// THREAT[TM-EFF-003]
fn exact_retry_replays_the_receipt_without_a_second_commit() {
    let controller = counter_controller(16);
    let request = request("agent-a", "request-a", 1, 2);

    let first = controller.execute(request.clone()).unwrap();
    let replay = controller.execute(request).unwrap();

    assert!(!first.replayed);
    assert!(replay.replayed);
    assert_eq!(first.outcome, replay.outcome);
    assert_eq!(controller.observe().unwrap().version, 2);
}

#[test]
// THREAT[TM-EFF-003]
fn version_cas_prevents_duplicate_commit_after_receipt_eviction() {
    let controller = counter_controller(1);
    let old_request = request("agent-a", "request-a", 1, 1);
    controller.execute(old_request.clone()).unwrap();
    controller
        .execute(request("agent-b", "request-b", 2, 2))
        .unwrap();

    let retry = controller.execute(old_request).unwrap();

    assert!(matches!(retry.outcome, ControlOutcome::Conflict { .. }));
    assert_eq!(controller.observe().unwrap().version, 3);
    let restored = Process::restore(&controller.snapshot().unwrap()).unwrap();
    assert_eq!(
        restored.read("/memory/count").unwrap(),
        Some(&Value::Integer(3))
    );
}

#[test]
fn reusing_a_request_id_with_different_content_is_rejected() {
    let controller = counter_controller(16);
    controller
        .execute(request("agent-a", "request-a", 1, 1))
        .unwrap();

    let response = controller
        .execute(request("agent-a", "request-a", 2, 100))
        .unwrap();

    let ControlOutcome::Rejected { failure, .. } = response.outcome else {
        panic!("request id reuse must be rejected");
    };
    assert_eq!(failure.code, "request_id_reused");
    assert_eq!(controller.observe().unwrap().version, 2);
}

#[test]
fn rejected_request_is_stable_under_retry() {
    let controller = counter_controller(16);
    let request = ControlRequest::activate(
        "agent-a",
        "missing-script",
        ProcessId::new("svit://local/control/counter").unwrap(),
        1,
        "missing",
        Value::Null,
    )
    .unwrap();

    let first = controller.execute(request.clone()).unwrap();
    let replay = controller.execute(request).unwrap();

    assert!(matches!(first.outcome, ControlOutcome::Rejected { .. }));
    assert_eq!(first.outcome, replay.outcome);
    assert!(replay.replayed);
    assert_eq!(controller.observe().unwrap().version, 1);
}

#[test]
fn request_and_response_have_a_stable_json_round_trip() {
    let controller = counter_controller(16);
    let request = request("agent-a", "request-a", 1, 4);
    let encoded_request = serde_json::to_vec(&request).unwrap();
    let decoded_request: ControlRequest = serde_json::from_slice(&encoded_request).unwrap();
    assert_eq!(decoded_request, request);

    let response = controller.execute(decoded_request).unwrap();
    let encoded_response = serde_json::to_vec(&response).unwrap();
    let decoded_response: ControlResponse = serde_json::from_slice(&encoded_response).unwrap();
    assert_eq!(decoded_response, response);
}

#[test]
fn activation_request_has_the_documented_wire_shape() {
    let encoded = serde_json::to_value(request("agent-a", "request-a", 1, 4)).unwrap();

    assert_eq!(
        encoded,
        serde_json::json!({
            "protocol": "svit-control@1",
            "client_id": "agent-a",
            "request_id": "request-a",
            "process_id": "svit://local/control/counter",
            "expected_version": 1,
            "command": {
                "operation": "activate",
                "script": "counter",
                "input": {
                    "type": "map",
                    "value": {
                        "by": { "type": "integer", "value": 4 }
                    }
                }
            }
        })
    );
}

#[test]
fn known_protocol_major_ignores_additive_fields() {
    let controller = counter_controller(16);
    let request = request("agent-a", "request-a", 1, 4);
    let mut encoded_request = serde_json::to_value(&request).unwrap();
    let request_object = encoded_request.as_object_mut().unwrap();
    request_object.insert("future_envelope_field".into(), true.into());
    request_object
        .get_mut("command")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("future_command_field".into(), 42.into());
    let decoded_request: ControlRequest = serde_json::from_value(encoded_request).unwrap();
    assert_eq!(decoded_request, request);

    let response = controller.execute(decoded_request).unwrap();
    let mut encoded_response = serde_json::to_value(&response).unwrap();
    let response_object = encoded_response.as_object_mut().unwrap();
    response_object.insert("future_response_field".into(), true.into());
    response_object
        .get_mut("outcome")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("future_outcome_field".into(), 42.into());
    response_object
        .get_mut("outcome")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .get_mut("activation")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("future_activation_field".into(), true.into());
    let decoded_response: ControlResponse = serde_json::from_value(encoded_response).unwrap();
    assert_eq!(decoded_response, response);
}

#[test]
fn unknown_operations_fail_closed() {
    let request = request("agent-a", "request-a", 1, 4);
    let mut encoded = serde_json::to_value(request).unwrap();
    encoded
        .as_object_mut()
        .unwrap()
        .get_mut("command")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert("operation".into(), "future_operation".into());

    assert!(serde_json::from_value::<ControlRequest>(encoded).is_err());
}

#[test]
// THREAT[TM-AUTH-001]
fn wire_decoder_revalidates_process_and_control_identifiers() {
    let invalid_process = br#"{
        "protocol":"svit-control@1",
        "client_id":"agent-a",
        "request_id":"request-a",
        "process_id":"not-an-address",
        "expected_version":0,
        "command":{"operation":"activate","script":"main","input":{"type":"Null"}}
    }"#;
    assert!(serde_json::from_slice::<ControlRequest>(invalid_process).is_err());

    let mut invalid_client = invalid_process.to_vec();
    let text = String::from_utf8(std::mem::take(&mut invalid_client))
        .unwrap()
        .replace("not-an-address", "svit://local/control/counter")
        .replace("agent-a", "agent/a");
    assert!(serde_json::from_str::<ControlRequest>(&text).is_err());
}

#[test]
// THREAT[TM-EFF-004]
fn independent_controllers_do_not_form_a_distributed_lease() {
    let controller = counter_controller(16);
    let snapshot = controller.snapshot().unwrap();
    let first = ProcessController::new(Process::restore(&snapshot).unwrap());
    let second = ProcessController::new(Process::restore(&snapshot).unwrap());

    let first_response = first.execute(request("host-a", "request-a", 1, 1)).unwrap();
    let second_response = second
        .execute(request("host-b", "request-b", 1, 1))
        .unwrap();

    assert!(matches!(
        first_response.outcome,
        ControlOutcome::Committed { .. }
    ));
    assert!(matches!(
        second_response.outcome,
        ControlOutcome::Committed { .. }
    ));
    assert_eq!(first.observe().unwrap().version, 2);
    assert_eq!(second.observe().unwrap().version, 2);
}

#[test]
// THREAT[TM-DOS-004]
fn receipt_retention_has_a_hard_host_limit() {
    let process = Process::new("svit://local/control/limits").unwrap();
    assert!(ProcessController::with_receipt_limit(process, 0).is_err());
    let process = Process::new("svit://local/control/limits").unwrap();
    assert!(ProcessController::with_receipt_limit(process, 4097).is_err());
}

#[test]
// THREAT[TM-DOS-005]
fn oversized_decoded_input_is_rejected_without_receipt_retention() {
    let controller = counter_controller(16);
    let process_id = ProcessId::new("svit://local/control/counter").unwrap();
    let oversized = ControlRequest::activate(
        "agent-a",
        "oversized",
        process_id.clone(),
        1,
        "counter",
        value!({"text": "x".repeat(1024 * 1024 + 1)}),
    )
    .unwrap();
    let response = controller.execute(oversized).unwrap();
    assert!(matches!(response.outcome, ControlOutcome::Rejected { .. }));

    let valid = ControlRequest::activate(
        "agent-a",
        "oversized",
        process_id,
        1,
        "counter",
        value!({"by": 1}),
    )
    .unwrap();
    assert!(matches!(
        controller.execute(valid).unwrap().outcome,
        ControlOutcome::Committed { .. }
    ));
}
