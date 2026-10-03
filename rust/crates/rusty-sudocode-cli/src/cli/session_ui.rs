//! Session selection and confirmation through the REPL's existing InputSlot.

use std::sync::{Arc, Mutex};

use crate::repl_ui::{OutputSender, QuestionOptionView, QuestionPromptView, UiCommandSender};
use crate::{show_slash_selection, LiveCli, SlashSelectionHandler};

use super::session::{format_session_picker_entry, list_managed_sessions};

/// Show a searchable session picker without giving another reader the TTY.
pub(crate) fn select_session(
    ui: &UiCommandSender,
    cli: &Arc<Mutex<LiveCli>>,
    out: &OutputSender,
) -> Option<SlashSelectionHandler> {
    let result = (|| -> Result<_, Box<dyn std::error::Error>> {
        let cli = cli.lock().expect("LiveCli mutex poisoned");
        cli.persist_session()?;
        Ok((cli.lifecycle.session_handle().id, list_managed_sessions()?))
    })();
    let (current, sessions) = match result {
        Ok(value) => value,
        Err(error) => {
            out.println(&error.to_string());
            return None;
        }
    };
    if sessions.is_empty() {
        out.println("No managed sessions saved yet.");
        return None;
    }
    let options = sessions
        .iter()
        .map(|session| QuestionOptionView {
            label: format_session_picker_entry(session, &current),
            value: session.id.clone(),
            description: None,
            recommended: session.id == current,
            is_navigable: false,
        })
        .collect();
    Some(show_slash_selection(
        ui,
        QuestionPromptView {
            title: Some("Sessions".into()),
            description: Some("Type to filter; Enter to switch; Esc to cancel".into()),
            index: 0,
            total: 1,
            prompt: "Select a session".into(),
            options,
            allow_custom_input: false,
            custom_input_hint: None,
            force_fuzzy_select: true,
            back_value: None,
        },
        sessions.into_iter().map(|session| session.id).collect(),
        move |target, cli, out| {
            if target == current {
                out.println(&format!("Session unchanged (already active: {target})."));
            } else {
                run_session_action(cli, out, "switch", &target);
            }
            None
        },
    ))
}

/// Confirm deletion through InputSlot; the lifecycle rechecks the active ID
/// when the answer arrives before deleting anything.
pub(crate) fn confirm_delete(
    ui: &UiCommandSender,
    cli: &Arc<Mutex<LiveCli>>,
    out: &OutputSender,
    target: &str,
) -> Option<SlashSelectionHandler> {
    let handle = match engine_host::session::resolve_session_reference(target) {
        Ok(handle) => handle,
        Err(error) => {
            out.println(&error.to_string());
            return None;
        }
    };
    if handle.id
        == cli
            .lock()
            .expect("LiveCli mutex poisoned")
            .lifecycle
            .session_handle()
            .id
    {
        out.println(
            "delete: refusing to delete the active session. Switch to another session first.",
        );
        return None;
    }
    let options = [("Cancel", "cancel"), ("Delete", "delete")]
        .into_iter()
        .map(|(label, value)| QuestionOptionView {
            label: label.into(),
            value: value.into(),
            description: None,
            recommended: value == "cancel",
            is_navigable: false,
        })
        .collect();
    Some(show_slash_selection(
        ui,
        QuestionPromptView {
            title: Some("Delete session".into()),
            description: Some("This cannot be undone.".into()),
            index: 0,
            total: 1,
            prompt: format!("Delete session '{}' ?", handle.id),
            options,
            allow_custom_input: false,
            custom_input_hint: None,
            force_fuzzy_select: false,
            back_value: None,
        },
        vec!["cancel".into(), "delete".into()],
        move |answer, cli, out| {
            if answer == "delete" {
                run_session_action(cli, out, "delete-force", &handle.id);
            } else {
                out.println("delete: cancelled.");
            }
            None
        },
    ))
}

fn run_session_action(cli: &Arc<Mutex<LiveCli>>, out: &OutputSender, action: &str, target: &str) {
    let mut cli = cli.lock().expect("LiveCli mutex poisoned");
    let result = cli.handle_session_command(Some(action), Some(target));
    let result = result.and_then(|changed| {
        if changed {
            cli.persist_session()
        } else {
            Ok(())
        }
    });
    if let Err(error) = result {
        out.println(&error.to_string());
    }
}
