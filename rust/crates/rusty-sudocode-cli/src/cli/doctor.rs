use crate::CliOutputFormat;

// The doctor report gatherer + diagnostic checks + render now live in
// `commands::reports`, shared with the ACP renderer so both render the same
// report from one definition. Re-exported here so the crate's existing
// `cli::doctor::render_doctor_report` import path keeps resolving.
pub(crate) use commands::reports::render_doctor_report;

pub(crate) fn run_doctor(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let report = render_doctor_report(&crate::build_info())?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => {
            println!("{message}");
            if report.has_failures() {
                return Err("doctor found failing checks".into());
            }
        }
        CliOutputFormat::Json => {
            // Emit a single valid JSON object that includes both the report
            // and the failure status so downstream tools never see a split
            // stdout-report + stderr-error pair (#121).
            println!("{}", serde_json::to_string_pretty(&report.json_value())?);
            if report.has_failures() {
                std::process::exit(1);
            }
        }
    }
    Ok(())
}
