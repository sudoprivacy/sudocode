//! Opt-in acceptance target; original individual Cargo test targets stay available.

#![allow(
    clippy::duplicate_mod,
    reason = "Original test modules keep their own helper namespaces."
)]

#[path = "../experiments_config_install.rs"]
mod experiments_config_install;

#[path = "../experiments_flags.rs"]
mod experiments_flags;

#[path = "../integration_tests.rs"]
mod integration_tests;

#[path = "../model_discovery_http.rs"]
mod model_discovery_http;

#[path = "../same_machine_pair.rs"]
mod same_machine_pair;

#[path = "../sudocode_config_extra_body.rs"]
mod sudocode_config_extra_body;

#[path = "../task_notification_render.rs"]
mod task_notification_render;
