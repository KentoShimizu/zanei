//! One bounded stdin request and one buffered stdout response, without normal CLI startup.
use super::context_wire::{
    self as wire, IncompatibleReason, Request, Result as Reply, UnavailableReason,
};
use crate::error::CliError;
use crate::store_access::{self, KeyPrompt};
use std::io::{Read, Write};
use std::path::Path;
use zanei_core::config::Config;
use zanei_core::store::{
    ContextPageError, ContextPageRequest, ContextPageResult, EvidenceContent, EvidenceRequest,
    EvidenceResult, StoreError, StoreFailureKind, StoreFormat, StoreReader,
};

pub fn run(config: Option<&Path>, store: Option<&Path>) -> Result<u8, CliError> {
    let bytes =
        respond(config, store).or_else(|reply| wire::encode(reply).map_err(CliError::Json))?;
    std::io::stdout()
        .lock()
        .write_all(&bytes)
        .map_err(CliError::Input)?;
    Ok(super::EXIT_SUCCESS)
}

fn respond(config: Option<&Path>, store: Option<&Path>) -> Result<Vec<u8>, Reply<'static>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take((wire::MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable(UnavailableReason::Input))?;
    if bytes.len() > wire::MAX_INPUT_BYTES {
        return Err(Reply::InvalidRequest);
    }
    let request: Request = serde_json::from_slice(&bytes).map_err(|_| Reply::InvalidRequest)?;
    if request.version() != wire::PROTOCOL_VERSION {
        return Err(Reply::Incompatible {
            reason: IncompatibleReason::Protocol,
            version: Some(request.version()),
        });
    }
    let (config, store) = config.zip(store).ok_or(Reply::InvalidRequest)?;
    let config =
        std::fs::read_to_string(config).map_err(|_| unavailable(UnavailableReason::Config))?;
    let config = Config::from_toml(&config).map_err(|_| unavailable(UnavailableReason::Config))?;
    store_access::initialize_key_environment().map_err(|_| unavailable(UnavailableReason::Key))?;
    let format = StoreFormat::probe(store).map_err(store_error)?;
    if format == StoreFormat::Missing {
        return Err(unavailable(UnavailableReason::Store));
    }
    let key = store_access::store_key_for(store, KeyPrompt::Suppressed).map_err(store_error)?;
    let reader = StoreReader::open_known(store, format, key.as_ref()).map_err(store_error)?;
    let retention = config.output.retention_hours;
    match request {
        Request::Page {
            cursor,
            upper_bound,
            limit,
            ..
        } => {
            let request = ContextPageRequest {
                cursor: cursor
                    .as_deref()
                    .map(wire::cursor)
                    .transpose()
                    .map_err(|_| Reply::InvalidRequest)?,
                upper_bound: upper_bound
                    .as_deref()
                    .map(wire::cursor)
                    .transpose()
                    .map_err(|_| Reply::InvalidRequest)?,
                limit,
            };
            match reader
                .read_context_page(&request, retention)
                .map_err(context_error)?
            {
                ContextPageResult::Page(page) => {
                    let full = wire::page(&page).map_err(encoding_error)?;
                    if full.len() <= wire::MAX_PAGE_BYTES {
                        return Ok(full);
                    }
                    // Measure candidate prefixes in memory; never re-read a changing store.
                    let mut low = 1;
                    let mut high = page.observations.len();
                    let mut best = None;
                    while low < high {
                        let count = low + (high - low) / 2;
                        let mut prefix = page.clone();
                        prefix.retain_prefix(count).map_err(context_error)?;
                        let bytes = wire::page(&prefix).map_err(encoding_error)?;
                        if bytes.len() <= wire::MAX_PAGE_BYTES {
                            best = Some(bytes);
                            low = count + 1;
                        } else {
                            high = count;
                        }
                    }
                    best.ok_or_else(|| unavailable(UnavailableReason::Store))
                }
                ContextPageResult::Gap(gap) => {
                    let bytes = wire::gap(&gap).map_err(encoding_error)?;
                    if bytes.len() > wire::MAX_PAGE_BYTES {
                        return Err(unavailable(UnavailableReason::Store));
                    }
                    Ok(bytes)
                }
                ContextPageResult::Incompatible { version } => Err(schema(version)),
            }
        }
        Request::Evidence {
            origin, start, end, ..
        } => {
            let request = EvidenceRequest {
                origin,
                start,
                end,
                max_bytes: wire::MAX_EVIDENCE_BYTES,
            };
            match reader
                .read_evidence(&request, retention)
                .map_err(context_error)?
            {
                EvidenceResult::Evidence(value) => {
                    let text = match &value.content {
                        EvidenceContent::Text { text, .. } => text.as_str(),
                        EvidenceContent::Absent => "",
                    };
                    let full = wire::evidence(&value, text.len()).map_err(encoding_error)?;
                    if full.len() <= wire::MAX_EVIDENCE_BYTES {
                        return Ok(full);
                    }
                    let boundaries: Vec<usize> = text
                        .char_indices()
                        .map(|(i, _)| i)
                        .filter(|i| *i > 0)
                        .collect();
                    let (mut low, mut high) = (0, boundaries.len());
                    let mut best = None;
                    while low < high {
                        let mid = low + (high - low) / 2;
                        let bytes =
                            wire::evidence(&value, boundaries[mid]).map_err(encoding_error)?;
                        if bytes.len() <= wire::MAX_EVIDENCE_BYTES {
                            best = Some(bytes);
                            low = mid + 1;
                        } else {
                            high = mid;
                        }
                    }
                    best.ok_or_else(|| unavailable(UnavailableReason::Store))
                }
                EvidenceResult::Expired => Err(Reply::Expired),
                EvidenceResult::Denied => Err(Reply::Denied),
                EvidenceResult::Incompatible { version } => Err(schema(version)),
            }
        }
    }
}
fn unavailable(reason: UnavailableReason) -> Reply<'static> {
    Reply::Unavailable { reason }
}
fn schema(version: i64) -> Reply<'static> {
    Reply::Incompatible {
        reason: IncompatibleReason::StoreSchema,
        version: Some(version),
    }
}
fn encoding_error(_: serde_json::Error) -> Reply<'static> {
    unavailable(UnavailableReason::Store)
}
fn context_error(error: ContextPageError) -> Reply<'static> {
    match error {
        ContextPageError::InvalidRequest(_) => Reply::InvalidRequest,
        ContextPageError::Store(error) => store_error(error),
    }
}
fn store_error(error: StoreError) -> Reply<'static> {
    if let StoreError::UnsupportedSchemaVersion(version) = error {
        return schema(version);
    }
    match error.failure_kind() {
        StoreFailureKind::Locked => unavailable(UnavailableReason::Key),
        StoreFailureKind::Unavailable => unavailable(UnavailableReason::Store),
        StoreFailureKind::Corrupt => Reply::Incompatible {
            reason: IncompatibleReason::StoreCorrupt,
            version: None,
        },
    }
}
