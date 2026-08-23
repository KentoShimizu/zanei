//! Event output boundaries.

use std::io::Write;

use crate::schema::Event;

#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    #[error("failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("failed to write event stream: {0}")]
    Io(#[from] std::io::Error),
    #[cfg(feature = "write")]
    #[error("failed to write event store: {0}")]
    Store(#[from] crate::store::StoreError),
}

pub trait Sink {
    fn write(&mut self, event: &Event) -> Result<(), SinkError>;

    fn flush(&mut self) -> Result<(), SinkError>;
}

pub struct StreamSink<W> {
    writer: W,
}

impl<W> StreamSink<W> {
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    pub fn into_inner(self) -> W {
        self.writer
    }
}

impl<W: Write> Sink for StreamSink<W> {
    fn write(&mut self, event: &Event) -> Result<(), SinkError> {
        let encoded = serde_json::to_vec(event)?;
        self.writer.write_all(&encoded)?;
        self.writer.write_all(b"\n")?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        self.writer.flush()?;
        Ok(())
    }
}

#[cfg(feature = "write")]
pub struct StoreSink {
    writer: crate::store::StoreWriter,
}

#[cfg(feature = "write")]
impl StoreSink {
    pub const fn new(writer: crate::store::StoreWriter) -> Self {
        Self { writer }
    }

    pub fn into_inner(self) -> crate::store::StoreWriter {
        self.writer
    }
}

#[cfg(feature = "write")]
impl Sink for StoreSink {
    fn write(&mut self, event: &Event) -> Result<(), SinkError> {
        self.writer.append(event)?;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::normalize::normalize;
    use crate::schema::{App, CaptureContext, EmptyData, EventData, RawEvent};
    use time::OffsetDateTime;

    use super::{Sink, StreamSink};

    #[test]
    fn stream_sink_writes_one_json_event_per_line() {
        let event = normalize(
            RawEvent {
                source: "macos.workspace".to_owned(),
                event_type: "app.launch".to_owned(),
                app: App {
                    name: "Finder".to_owned(),
                    bundle_id: Some("com.apple.finder".to_owned()),
                    pid: Some(1),
                },
                window: None,
                element: None,
                data: EventData::AppLaunch(EmptyData::default()),
                capture_context: CaptureContext::default(),
            },
            OffsetDateTime::UNIX_EPOCH,
            0,
        )
        .expect("fixture wall time is representable");
        let mut sink = StreamSink::new(Vec::new());

        sink.write(&event.event).expect("event should serialize");
        sink.flush().expect("memory writer should flush");
        let bytes = sink.into_inner();

        assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        let decoded: crate::schema::Event =
            serde_json::from_slice(&bytes).expect("line should contain one event");
        assert_eq!(decoded, event.event);
    }
}
