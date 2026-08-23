//! macOS application discovery for CLI selection and filter resolution.

use std::fs;
use std::path::{Path, PathBuf};

use zanei_collector::{AppDirectory, AppDirectoryError, AppInfo};

use crate::ffi::app_directory::{
    BundleMetadata, application_path_for_bundle_id, ensure_workspace_available, home_directory,
    parse_info_plist,
};
use crate::ffi::workspace::{
    NativeApplication, NativeApplicationActivationPolicy, running_applications,
};

const INFO_PLIST_RELATIVE_PATH: &str = "Contents/Info.plist";

#[derive(Default)]
pub struct MacosAppDirectory;

impl AppDirectory for MacosAppDirectory {
    fn installed(&self) -> Result<Vec<AppInfo>, AppDirectoryError> {
        let home = home_directory().map_err(platform_error)?;
        installed_in_roots(&[
            PathBuf::from("/Applications"),
            home.join("Applications"),
            PathBuf::from("/System/Applications"),
        ])
    }

    fn running(&self) -> Result<Vec<AppInfo>, AppDirectoryError> {
        ensure_workspace_available().map_err(platform_error)?;
        Ok(running_applications()
            .into_iter()
            .filter_map(running_app_info)
            .collect())
    }

    fn installed_by_id(&self, bundle_id: &str) -> Result<Option<AppInfo>, AppDirectoryError> {
        installed_by_id_with(bundle_id, application_path_for_bundle_id)
    }
}

fn installed_in_roots(roots: &[PathBuf]) -> Result<Vec<AppInfo>, AppDirectoryError> {
    let mut bundle_paths = Vec::new();
    for root in roots {
        collect_bundle_paths(root, &mut bundle_paths)?;
    }
    bundle_paths.sort();
    bundle_paths
        .iter()
        .map(|path| read_app_info(path))
        .collect()
}

fn collect_bundle_paths(
    root: &Path,
    bundle_paths: &mut Vec<PathBuf>,
) -> Result<(), AppDirectoryError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(AppDirectoryError::file_system(
                "list application directory",
                root,
                source,
            ));
        }
    };
    for entry in entries {
        let entry = entry.map_err(|source| {
            AppDirectoryError::file_system("read application directory entry", root, source)
        })?;
        let path = entry.path();
        if is_app_bundle(&path) {
            bundle_paths.push(path);
            continue;
        }
        let file_type = entry.file_type().map_err(|source| {
            AppDirectoryError::file_system("inspect application directory entry", &path, source)
        })?;
        if file_type.is_dir() {
            collect_nested_bundle_paths(&path, bundle_paths)?;
        }
    }
    Ok(())
}

fn collect_nested_bundle_paths(
    directory: &Path,
    bundle_paths: &mut Vec<PathBuf>,
) -> Result<(), AppDirectoryError> {
    let entries = fs::read_dir(directory).map_err(|source| {
        AppDirectoryError::file_system("list nested application directory", directory, source)
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            AppDirectoryError::file_system(
                "read nested application directory entry",
                directory,
                source,
            )
        })?;
        let path = entry.path();
        if is_app_bundle(&path) {
            bundle_paths.push(path);
        }
    }
    Ok(())
}

fn is_app_bundle(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
}

fn read_app_info(bundle_path: &Path) -> Result<AppInfo, AppDirectoryError> {
    let plist_path = bundle_path.join(INFO_PLIST_RELATIVE_PATH);
    let bytes = fs::read(&plist_path).map_err(|source| {
        AppDirectoryError::file_system("read application Info.plist", &plist_path, source)
    })?;
    let metadata =
        parse_info_plist(&bytes).map_err(|error| AppDirectoryError::InvalidMetadata {
            path: plist_path,
            reason: error.to_string(),
        })?;
    Ok(app_info_from_metadata(bundle_path, metadata))
}

fn app_info_from_metadata(bundle_path: &Path, metadata: BundleMetadata) -> AppInfo {
    let folder_name = bundle_path
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| bundle_path.display().to_string());
    AppInfo {
        name: nonempty(metadata.display_name)
            .or_else(|| nonempty(metadata.bundle_name))
            .unwrap_or(folder_name),
        bundle_id: nonempty(metadata.bundle_id),
        path: Some(bundle_path.to_path_buf()),
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn running_app_info(app: NativeApplication) -> Option<AppInfo> {
    match app.activation_policy {
        NativeApplicationActivationPolicy::Regular
        | NativeApplicationActivationPolicy::Accessory => Some(AppInfo {
            name: app.name,
            bundle_id: app.bundle_id,
            path: None,
        }),
        NativeApplicationActivationPolicy::Prohibited => None,
    }
}

fn installed_by_id_with(
    bundle_id: &str,
    lookup: impl FnOnce(
        &str,
    )
        -> Result<Option<PathBuf>, crate::ffi::app_directory::NativeAppDirectoryError>,
) -> Result<Option<AppInfo>, AppDirectoryError> {
    lookup(bundle_id)
        .map_err(platform_error)?
        .map(|path| read_app_info(&path))
        .transpose()
}

fn platform_error(error: impl std::fmt::Display) -> AppDirectoryError {
    AppDirectoryError::Platform(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn plist_name_precedence_uses_display_then_bundle_then_folder() {
        let directory = TempDir::new().expect("temporary application directory");
        let display = create_app(
            directory.path(),
            "Display.app",
            Some("dev.example.display"),
            Some("Localized Display"),
            Some("Bundle Name"),
        );
        let bundle = create_app(
            directory.path(),
            "Bundle.app",
            None,
            None,
            Some("Bundle Name"),
        );
        let folder = create_app(directory.path(), "Folder.app", None, None, None);

        assert_eq!(
            read_app_info(&display).expect("display metadata").name,
            "Localized Display"
        );
        assert_eq!(
            read_app_info(&bundle).expect("bundle metadata").name,
            "Bundle Name"
        );
        assert_eq!(
            read_app_info(&folder).expect("folder metadata").name,
            "Folder"
        );
    }

    #[test]
    fn installed_scan_stops_after_one_nested_directory() {
        let directory = TempDir::new().expect("temporary application directory");
        create_app(directory.path(), "Direct.app", None, None, None);
        let utilities = directory.path().join("Utilities");
        create_app(&utilities, "Nested.app", None, None, None);
        create_app(&utilities.join("Deeper"), "Ignored.app", None, None, None);

        let apps = installed_in_roots(&[directory.path().to_path_buf()]).expect("installed apps");
        let names: Vec<_> = apps.into_iter().map(|app| app.name).collect();
        assert_eq!(names, ["Direct", "Nested"]);
    }

    #[test]
    fn running_apps_include_regular_and_accessory_only() {
        let app = |activation_policy| NativeApplication {
            name: "Fixture".to_owned(),
            bundle_id: Some("dev.example.fixture".to_owned()),
            pid: 1,
            activation_policy,
        };

        assert!(running_app_info(app(NativeApplicationActivationPolicy::Regular)).is_some());
        assert!(running_app_info(app(NativeApplicationActivationPolicy::Accessory)).is_some());
        assert!(running_app_info(app(NativeApplicationActivationPolicy::Prohibited)).is_none());
    }

    #[test]
    fn launch_services_failure_is_not_treated_as_not_installed() {
        let result = installed_by_id_with("dev.example.fixture", |_| {
            Err(crate::ffi::app_directory::NativeAppDirectoryError::new(
                "fixture LaunchServices failure",
            ))
        });

        assert!(matches!(
            result,
            Err(AppDirectoryError::Platform(reason)) if reason.contains("fixture LaunchServices failure")
        ));
    }

    fn create_app(
        parent: &Path,
        folder: &str,
        bundle_id: Option<&str>,
        display_name: Option<&str>,
        bundle_name: Option<&str>,
    ) -> PathBuf {
        let path = parent.join(folder);
        fs::create_dir_all(path.join("Contents")).expect("application bundle directory");
        let mut entries = String::new();
        for (key, value) in [
            ("CFBundleIdentifier", bundle_id),
            ("CFBundleDisplayName", display_name),
            ("CFBundleName", bundle_name),
        ] {
            if let Some(value) = value {
                entries.push_str(&format!("<key>{key}</key><string>{value}</string>"));
            }
        }
        fs::write(
            path.join(INFO_PLIST_RELATIVE_PATH),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict>{entries}</dict></plist>"
            ),
        )
        .expect("application Info.plist");
        path
    }
}
