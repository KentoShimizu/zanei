use std::{env, fs, io, path::Path, path::PathBuf};

pub(crate) fn current() -> io::Result<PathBuf> {
    let executable = env::current_exe()?;
    Ok(canonicalize_or_original(&executable))
}

pub(crate) fn canonicalize(executable: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(executable)
}

pub(crate) fn canonicalize_or_original(executable: &Path) -> PathBuf {
    canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::canonicalize_or_original;

    #[test]
    fn unresolved_executable_keeps_its_original_path() {
        let directory = TempDir::new().expect("unresolved executable fixture");
        let executable = directory.path().join("missing/zanei");

        assert_eq!(canonicalize_or_original(&executable), executable);
    }
}
