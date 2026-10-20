/// Read tool history from the actual wire format, preserving IDs and errors.
pub fn request_tool_blocks(request: &serde_json::Value) -> Vec<serde_json::Value> {
    use serde_json::{json, Value};
    let mut blocks = Vec::new();
    for message in request["messages"].as_array().into_iter().flatten() {
        if let Some(content) = message["content"].as_array() {
            blocks.extend(
                content
                    .iter()
                    .filter(|b| matches!(b["type"].as_str(), Some("tool_use" | "tool_result")))
                    .cloned(),
            );
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            blocks.push(json!({"type":"tool_use","id":call["id"],"name":call["function"]["name"],"input":serde_json::from_str::<Value>(call["function"]["arguments"].as_str().expect("tool arguments string")).expect("valid wire tool arguments")}));
        }
        if message["role"] == "tool" {
            blocks.push(json!({"type":"tool_result","tool_use_id":message["tool_call_id"],"content":message["content"],"is_error":message["is_error"]}));
        }
    }
    for item in request["input"].as_array().into_iter().flatten() {
        if item["type"] == "function_call" {
            blocks.push(json!({"type":"tool_use","id":item["call_id"],"name":item["name"],"input":serde_json::from_str::<Value>(item["arguments"].as_str().expect("tool arguments string")).expect("valid wire tool arguments")}));
        } else if item["type"] == "function_call_output" {
            blocks.push(json!({"type":"tool_result","tool_use_id":item["call_id"],"content":item["output"]}));
        }
    }
    blocks
}

pub fn is_inference_request_dump(path: &std::path::Path) -> bool {
    path.file_name().is_some_and(|name| {
        let name = name.to_string_lossy();
        [
            "-messages.json",
            "-chat-completions.json",
            "-responses.json",
        ]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    })
}
