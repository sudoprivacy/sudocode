//! Native `scode browser` CLI. Runs before model credentials/config loading.
#[rustfmt::skip]
#[allow(clippy::all, clippy::pedantic)] // Unmodified, pinned upstream CLI adapter.
mod upstream;

use clap::Parser;
use std::io::Write;

#[derive(Parser)]
#[command(
    name = "scode browser",
    about = "Control Chrome/Chromium/Edge through the sudohand browser CLI",
    version
)]
struct BrowserCli {
    #[command(subcommand)]
    command: upstream::Cmd,
}

pub fn run(args: impl IntoIterator<Item = std::ffi::OsString>) -> i32 {
    let cli = match BrowserCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            return 0;
        }
        Err(error) => return fail(&sudohand_core::Error::invalid(error.to_string())),
    };
    // Raw page data may legitimately contain an "error" property.
    let raw_data = matches!(
        &cli.command,
        upstream::Cmd::JsEvaluate { .. }
            | upstream::Cmd::CdpSend { .. }
            | upstream::Cmd::StorageGet { .. }
    );
    match upstream::run(cli.command) {
        Ok(value) => {
            // Some upstream actuators return an embedded error instead of Err.
            // Never report that as a successful Bash call.
            if let Some(error) = value
                .get("error")
                .filter(|error| !raw_data && !error.is_null())
            {
                let message = error
                    .as_str()
                    .map_or_else(|| error.to_string(), str::to_owned);
                return fail(&sudohand_core::Error::io(message));
            }
            let _ = writeln!(std::io::stdout().lock(), "{value}");
            0
        }
        Err(error) => fail(&error),
    }
}

fn fail(error: &sudohand_core::Error) -> i32 {
    let _ = writeln!(std::io::stderr().lock(), "{}", error.envelope());
    i32::from(error.exit_code())
}
