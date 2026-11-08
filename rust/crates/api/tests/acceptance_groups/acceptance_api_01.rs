//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../client_integration.rs"]
mod client_integration;

#[path = "../openai_compat_integration.rs"]
mod openai_compat_integration;

#[path = "../openai_reasoning_http.rs"]
mod openai_reasoning_http;

#[path = "../provider_client_integration.rs"]
mod provider_client_integration;

#[path = "../proxy_integration.rs"]
mod proxy_integration;

#[path = "../reliability_integration.rs"]
mod reliability_integration;
