use std::{
    fs::{self, File},
    io::{BufWriter, IsTerminal, Read, Write},
    path::Path,
    sync::{Arc, atomic::AtomicBool, mpsc},
    thread,
};

use signal_hook::{
    SigId,
    consts::{SIGINT, SIGTERM},
    flag,
    low_level::unregister,
};

use super::{DaemonError, runtime::RecordOutput};

/// Creates the store's parent directory when it is missing. A directory Zanei
/// creates is owner-only; an existing directory (for example one chosen with
/// `--store`) keeps whatever permissions it has.
pub(crate) fn ensure_store_parent(store_path: &Path) -> Result<(), DaemonError> {
    let Some(parent) = store_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.is_dir() {
        return Ok(());
    }
    let file_error = |operation: &'static str, source| DaemonError::File {
        operation,
        path: store_path.to_owned(),
        source,
    };
    fs::create_dir_all(parent).map_err(|source| file_error("create directory for", source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|source| file_error("restrict the directory permissions of", source))?;
    }
    Ok(())
}

/// Makes an existing store (and its WAL companions) readable by the owner only.
pub(crate) fn restrict_store_permissions(store_path: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut companions = vec![store_path.to_owned()];
        for suffix in ["-wal", "-shm"] {
            let mut companion = store_path.as_os_str().to_os_string();
            companion.push(suffix);
            companions.push(companion.into());
        }
        for path in companions {
            match fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
                Ok(()) => {}
                Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(DaemonError::File {
                        operation: "restrict the permissions of",
                        path,
                        source,
                    });
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn record_writer(output: RecordOutput) -> Result<Box<dyn Write + Send>, DaemonError> {
    match output {
        RecordOutput::Stdout => Ok(Box::new(BufWriter::new(std::io::stdout()))),
        RecordOutput::File(path) => {
            let file = File::create(&path).map_err(|source| DaemonError::File {
                operation: "create",
                path,
                source,
            })?;
            Ok(Box::new(BufWriter::new(file)))
        }
    }
}

pub(crate) struct ShutdownSignals {
    stop: Arc<AtomicBool>,
    registrations: Vec<SigId>,
}

impl ShutdownSignals {
    pub(crate) fn install() -> Result<Self, DaemonError> {
        let stop = Arc::new(AtomicBool::new(false));
        let term = flag::register(SIGTERM, Arc::clone(&stop)).map_err(|source| {
            DaemonError::SignalRegistration {
                signal: "SIGTERM",
                source,
            }
        })?;
        let interrupt = match flag::register(SIGINT, Arc::clone(&stop)) {
            Ok(registration) => registration,
            Err(source) => {
                let _ = unregister(term);
                return Err(DaemonError::SignalRegistration {
                    signal: "SIGINT",
                    source,
                });
            }
        };
        Ok(Self {
            stop,
            registrations: vec![term, interrupt],
        })
    }

    pub(crate) fn stop_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }
}

impl Drop for ShutdownSignals {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            let _ = unregister(registration);
        }
    }
}

pub(crate) struct StdinEofWatcher {
    receiver: mpsc::Receiver<Result<(), std::io::Error>>,
}

impl StdinEofWatcher {
    pub(crate) fn start() -> Result<Option<Self>, DaemonError> {
        if std::io::stdin().is_terminal() {
            return Ok(None);
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("zanei-stdin-eof".to_owned())
            .spawn(move || {
                let mut stdin = std::io::stdin().lock();
                let mut buffer = [0_u8; 1_024];
                let result = loop {
                    match stdin.read(&mut buffer) {
                        Ok(0) => break Ok(()),
                        Ok(_) => {}
                        Err(error) => break Err(error),
                    }
                };
                let _ = sender.send(result);
            })
            .map_err(|source| DaemonError::ThreadSpawn {
                thread: "stdin EOF watcher",
                source,
            })?;
        Ok(Some(Self { receiver }))
    }

    pub(crate) fn try_result(&self) -> Option<Result<(), std::io::Error>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Ok(())),
        }
    }
}
