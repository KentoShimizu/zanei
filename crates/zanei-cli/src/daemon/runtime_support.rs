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

pub(crate) fn ensure_store_parent(store_path: &Path) -> Result<(), DaemonError> {
    let Some(parent) = store_path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|source| DaemonError::File {
        operation: "create directory for",
        path: store_path.to_owned(),
        source,
    })
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
