# Sourced by script/photon-netcheck and script/photon-perf.
#
# Launches Photon the way a user does (`open`), so macOS activates its window. A bare binary started from a
# background shell may stay behind another app; Photon then draws more and never starts the diagram web view.

# launch_as_app BINARY LOG ENV_ASSIGNMENTS... -- ARGS...
# Sets app_pid, and measure_dir when it had to wrap a bare binary in a temporary bundle.
launch_as_app() {
  local binary=${1:a} log=$2; shift 2
  local env_flags=()
  while [[ $1 != -- ]]; do env_flags+=(--env "$1"); shift; done
  shift
  local bundle
  if [[ $binary == *.app/Contents/MacOS/* ]]; then
    bundle=${binary%%.app/Contents/MacOS/*}.app
  else
    # Dev builds find their assets through the executable's ancestor .git, so the bundle lives in the repo.
    if [[ -z ${measure_dir:-} ]]; then
      mkdir -p target/measure
      measure_dir=$(mktemp -d "$PWD/target/measure/run.XXXXXX")
      mkdir -p "$measure_dir/Photon Measure.app/Contents/MacOS"
      cat >"$measure_dir/Photon Measure.app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.photon.measure</string>
<key>CFBundleName</key><string>Photon Measure</string>
<key>CFBundleExecutable</key><string>zed</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
      cp -c "$binary" "$measure_dir/Photon Measure.app/Contents/MacOS/zed"
      codesign --force --sign - "$measure_dir/Photon Measure.app" 2>/dev/null
    fi
    bundle="$measure_dir/Photon Measure.app"
  fi
  : >"$log"
  open -n -F --stdout "$log" --stderr "$log" "${env_flags[@]}" "$bundle" --args "$@"
  app_pid=""
  local attempt
  for (( attempt = 0; attempt < 100; attempt++ )); do
    app_pid=$(pgrep -n -f "$bundle/Contents/MacOS/zed" || true)
    [[ -n $app_pid ]] && return 0
    sleep 0.1
  done
  echo "could not find the launched app process for $bundle" >&2
  return 1
}
