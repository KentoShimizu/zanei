use std::thread;

use zanei_macos::main_run_loop::MainRunLoop;

use super::DaemonError;

pub(crate) fn prepare() -> Result<MainRunLoop, DaemonError> {
    MainRunLoop::new().map_err(DaemonError::from)
}

pub(crate) fn run<T, F>(
    run_loop: MainRunLoop,
    thread_name: &'static str,
    worker: F,
) -> Result<T, DaemonError>
where
    T: Send,
    F: FnOnce() -> T + Send,
{
    thread::scope(|scope| {
        let stopper = run_loop.stopper();
        let handle = thread::Builder::new()
            .name(format!("zanei-{thread_name}"))
            .spawn_scoped(scope, move || {
                let result = worker();
                stopper.stop();
                result
            })
            .map_err(|source| DaemonError::ThreadSpawn {
                thread: thread_name,
                source,
            })?;

        run_loop.run();
        handle.join().map_err(|_| DaemonError::ThreadTerminated {
            thread: thread_name,
        })
    })
}
