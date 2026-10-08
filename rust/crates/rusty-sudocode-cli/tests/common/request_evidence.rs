//! Match logical message requests rather than counting HTTP attempts.
use serde_json::Value;
use std::{collections::HashMap, path::Path};

/// Return one body per HTTP-accepted message request, in acceptance order.
/// The transport's request_id is stable across retries, unlike a body-count
/// heuristic. Every retry must preserve the whole body; every success must
/// have exactly one captured logical request. Failed calls (e.g. context
/// pressure) have no usage row. The caller still verifies complete usage and
/// full-prefix reuse for every accepted request, including compaction/resume.
pub fn accepted_messages(log: &Path) -> Vec<Value> {
    let events: Vec<Value> = std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let mut bodies = HashMap::new();
    for event in &events {
        let attributes = &event["attributes"];
        if event["event"] != "request_debug"
            || !attributes["body"]["messages"].is_array()
            || !attributes["url"]
                .as_str()
                .unwrap()
                .split('?')
                .next()
                .unwrap()
                .ends_with("/messages")
        {
            continue;
        }
        let id = attributes["request_id"]
            .as_str()
            .expect("message request has a correlation id");
        if let Some(body) = bodies.get(id) {
            assert_eq!(
                body, &attributes["body"],
                "retry changed a cache-relevant request body: {id}"
            );
        } else {
            bodies.insert(id, attributes["body"].clone());
        }
    }
    events
        .iter()
        .filter_map(|event| {
            let attributes = &event["attributes"];
            if event["event"] != "request_succeeded"
                || !attributes["path"].as_str().unwrap().ends_with("/messages")
            {
                return None;
            }
            assert!((200..300).contains(&attributes["status"].as_u64().unwrap()));
            let id = attributes["request_id"].as_str().unwrap();
            Some(
                bodies
                    .remove(id)
                    .expect("exactly one captured logical request per HTTP success"),
            )
        })
        .collect()
}
