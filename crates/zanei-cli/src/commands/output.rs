use std::io::Write;

use zanei_core::schema::Event;

use crate::error::CliError;

pub fn write_jsonl(events: &[Event], writer: &mut impl Write) -> Result<(), CliError> {
    for event in events {
        serde_json::to_writer(&mut *writer, event)?;
        writer.write_all(b"\n").map_err(CliError::Input)?;
    }
    Ok(())
}

pub fn write_json(events: &[Event], writer: &mut impl Write) -> Result<(), CliError> {
    serde_json::to_writer_pretty(&mut *writer, events)?;
    writer.write_all(b"\n").map_err(CliError::Input)?;
    Ok(())
}

pub fn write_table(events: &[Event], writer: &mut impl Write) -> Result<(), CliError> {
    writeln!(writer, "TIMESTAMP\tTYPE\tAPP\tBUNDLE ID\tWINDOW").map_err(CliError::Input)?;
    for event in events {
        let bundle_id = event.app.bundle_id.as_deref().unwrap_or("-");
        let window = event
            .window
            .as_ref()
            .and_then(|window| window.title.as_deref())
            .unwrap_or("-");
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            event.ts, event.event_type, event.app.name, bundle_id, window
        )
        .map_err(CliError::Input)?;
    }
    Ok(())
}
