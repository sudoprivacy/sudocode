//! Memory selection uses the same input owner as the rest of the REPL.

use std::sync::{Arc, Mutex};

use crate::repl_ui::{OutputSender, QuestionOptionView, QuestionPromptView, UiCommandSender};
use crate::{show_slash_selection, LiveCli, ProjectContext, SlashSelectionHandler};

pub(crate) fn select_memory(
    ui: &UiCommandSender,
    cli: &Arc<Mutex<LiveCli>>,
    out: &OutputSender,
) -> Option<SlashSelectionHandler> {
    let result = std::env::current_dir().and_then(|cwd| {
        ProjectContext::discover(&cwd, runtime::today_local()).map_err(std::io::Error::other)
    });
    let context = match result {
        Ok(context) => context,
        Err(error) => {
            out.println(&error.to_string());
            return None;
        }
    };
    if context.instruction_files.len() <= 1 {
        if let Err(error) = cli.lock().expect("LiveCli mutex poisoned").edit_memory() {
            out.println(&error.to_string());
        }
        return None;
    }
    let files: Vec<_> = context
        .instruction_files
        .into_iter()
        .map(|file| file.path)
        .collect();
    let values: Vec<_> = (0..files.len()).map(|index| index.to_string()).collect();
    let options = files
        .iter()
        .zip(&values)
        .map(|(path, value)| QuestionOptionView {
            label: path.display().to_string(),
            value: value.clone(),
            description: None,
            recommended: false,
            is_navigable: false,
        })
        .collect();
    Some(show_slash_selection(
        ui,
        QuestionPromptView {
            title: Some("Memory".into()),
            description: Some("Type to filter; Enter to edit; Esc to cancel".into()),
            index: 0,
            total: 1,
            prompt: "Select memory file to edit".into(),
            options,
            allow_custom_input: false,
            custom_input_hint: None,
            force_fuzzy_select: true,
            back_value: None,
        },
        values,
        move |value, cli, out| {
            if let Some(path) = value
                .parse::<usize>()
                .ok()
                .and_then(|index| files.get(index))
            {
                let cli = cli.lock().expect("LiveCli mutex poisoned");
                match cli.out_suspend(|| LiveCli::open_in_editor(path)) {
                    Ok(Ok(message)) => out.println(&message),
                    Ok(Err(error)) => out.println(&error.to_string()),
                    Err(error) => out.println(&error.to_string()),
                }
            }
            None
        },
    ))
}
