/// Select actual provider request bodies, excluding response and metadata files.
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
