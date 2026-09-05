#!/usr/bin/env bash
set -euo pipefail

readonly SCRIPT_DIRECTORY="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPOSITORY_ROOT="$(cd -- "${SCRIPT_DIRECTORY}/.." && pwd)"
readonly DEFAULT_BINARY_PATH="${REPOSITORY_ROOT}/target/release/zanei"
readonly DEFAULT_APP_PATH="${REPOSITORY_ROOT}/dist/Zanei.app"
readonly ENTITLEMENTS_PATH="${SCRIPT_DIRECTORY}/entitlements.plist"

STAGING_DIRECTORY=""

usage() {
  echo "usage: $0 [--timestamp] <codesign-identity> [binary-path] [app-path]" >&2
}

cleanup() {
  if [[ -n "${STAGING_DIRECTORY}" && -d "${STAGING_DIRECTORY}" ]]; then
    rm -rf -- "${STAGING_DIRECTORY}"
  fi
}

main() {
  local timestamp=false
  if [[ "${1:-}" == "--timestamp" ]]; then
    timestamp=true
    shift
  fi
  if (( $# < 1 || $# > 3 )); then
    usage
    return 2
  fi

  local -r identity="$1"
  local -r binary_path="${2:-${DEFAULT_BINARY_PATH}}"
  local -r app_path="${3:-${DEFAULT_APP_PATH}}"
  if [[ -z "${identity}" ]]; then
    echo "codesign identity must not be empty" >&2
    return 2
  fi
  if [[ ! -f "${binary_path}" || ! -x "${binary_path}" ]]; then
    echo "release binary is missing or not executable: ${binary_path}" >&2
    return 1
  fi
  if [[ "${app_path##*/}" != "Zanei.app" ]]; then
    echo "app output path must end with Zanei.app: ${app_path}" >&2
    return 2
  fi

  local version_output
  version_output=$("${binary_path}" --version)
  if [[ ! "${version_output}" =~ ^zanei[[:space:]]+([^[:space:]]+)$ ]]; then
    echo "could not read the Cargo package version from ${binary_path}: ${version_output}" >&2
    return 1
  fi
  local -r version="${BASH_REMATCH[1]}"

  local -r output_directory="$(dirname -- "${app_path}")"
  mkdir -p -- "${output_directory}"
  STAGING_DIRECTORY=$(mktemp -d "${output_directory}/.zanei-app.XXXXXX")
  local -r staged_app="${STAGING_DIRECTORY}/Zanei.app"
  local -r executable_directory="${staged_app}/Contents/MacOS"
  mkdir -p -- "${executable_directory}"
  install -m 755 -- "${binary_path}" "${executable_directory}/zanei"
  mkdir -p -- "${staged_app}/Contents/Resources"
  install -m 644 -- "${SCRIPT_DIRECTORY}/Zanei.icns" "${staged_app}/Contents/Resources/Zanei.icns"

  cat > "${staged_app}/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>zanei</string>
  <key>CFBundleIdentifier</key>
  <string>dev.zanei.recorder</string>
  <key>CFBundleName</key>
  <string>Zanei</string>
  <key>CFBundleIconFile</key>
  <string>Zanei.icns</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>LSUIElement</key>
  <true/>
  <key>NSAppleEventsUsageDescription</key>
  <string>Zanei reads Chrome URLs and window types to record browser activity.</string>
</dict>
</plist>
EOF

  /usr/bin/plutil -lint "${staged_app}/Contents/Info.plist"

  local -a codesign_arguments=(
    --force
    --options runtime
    --entitlements "${ENTITLEMENTS_PATH}"
    --sign "${identity}"
  )
  if [[ "${timestamp}" == true ]]; then
    codesign_arguments+=(--timestamp)
  fi
  /usr/bin/codesign "${codesign_arguments[@]}" "${staged_app}"
  /usr/bin/codesign --verify --strict --verbose=2 "${staged_app}"

  rm -rf -- "${app_path}"
  mv -- "${staged_app}" "${app_path}"
  cleanup
  STAGING_DIRECTORY=""
  printf 'Created %s\n' "${app_path}"
}

trap cleanup EXIT
main "$@"
