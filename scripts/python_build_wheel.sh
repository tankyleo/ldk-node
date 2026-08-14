#!/bin/bash
# Build and test one native Python wheel on the current host.
#
# Run this script once on each supported native target:
#   Linux x86_64, Linux aarch64, macOS x86_64, macOS arm64.
# Linux builds require Docker or Podman. macOS builds require Rust 1.95.0.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="$REPO_ROOT/bindings/python/wheelhouse"
ALLOW_DIRTY=false
CIBUILDWHEEL_VERSION="4.1.0"

usage() {
	echo "Usage: $0 [--output-dir DIR] [--allow-dirty]"
}

while [[ $# -gt 0 ]]; do
	case "$1" in
		--output-dir)
			[[ $# -ge 2 ]] || { usage >&2; exit 2; }
			OUTPUT_DIR="$2"
			shift 2
			;;
		--allow-dirty)
			ALLOW_DIRTY=true
			shift
			;;
		-h|--help)
			usage
			exit 0
			;;
		*)
			echo "Unknown argument: $1" >&2
			usage >&2
			exit 2
			;;
	esac
done

case "$OUTPUT_DIR" in
	/*) ;;
	*) OUTPUT_DIR="$REPO_ROOT/$OUTPUT_DIR" ;;
esac

for command_name in git uv; do
	if ! command -v "$command_name" >/dev/null 2>&1; then
		echo "Required command not found: $command_name" >&2
		exit 1
	fi
done

cd "$REPO_ROOT"

if [[ "$ALLOW_DIRTY" == false ]] && [[ -n "$(git status --porcelain --untracked-files=normal)" ]]; then
	echo "Refusing to build from a dirty worktree; commit the release first." >&2
	echo "Use --allow-dirty only while developing the build configuration." >&2
	exit 1
fi

case "$(uname -s)" in
	Linux)
		case "${CIBW_CONTAINER_ENGINE:-}" in
			podman*) CONTAINER_COMMAND=podman ;;
			docker*|"") CONTAINER_COMMAND=docker ;;
			*)
				echo "Unsupported CIBW_CONTAINER_ENGINE: $CIBW_CONTAINER_ENGINE" >&2
				exit 1
				;;
		esac

		if ! command -v "$CONTAINER_COMMAND" >/dev/null 2>&1; then
			if [[ -z "${CIBW_CONTAINER_ENGINE:-}" ]] && command -v podman >/dev/null 2>&1; then
				export CIBW_CONTAINER_ENGINE=podman
			else
				echo "Linux wheel builds require Docker or Podman." >&2
				exit 1
			fi
		fi
		;;
	Darwin)
		if ! command -v rustup >/dev/null 2>&1 || \
			! rustup run 1.95.0 rustc --version >/dev/null 2>&1; then
			echo "macOS wheel builds require the Rust 1.95.0 toolchain." >&2
			echo "Install it with: rustup toolchain install 1.95.0 --profile minimal" >&2
			exit 1
		fi
		;;
	*)
		echo "Unsupported operating system: $(uname -s)" >&2
		exit 1
		;;
esac

case "$(uname -m)" in
	x86_64|amd64|aarch64|arm64) ;;
	*)
		echo "Unsupported architecture: $(uname -m)" >&2
		exit 1
		;;
esac

mkdir -p "$OUTPUT_DIR"
shopt -s nullglob
existing_wheels=("$OUTPUT_DIR"/*.whl)
if [[ ${#existing_wheels[@]} -ne 0 ]]; then
	echo "Output directory already contains wheels: $OUTPUT_DIR" >&2
	exit 1
fi

CARGO_TARGET_DIR="$(mktemp -d /tmp/cargo-target-ldk-node-python-wheels.XXXXXX)"
export CARGO_TARGET_DIR
SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
export SOURCE_DATE_EPOCH

cleanup() {
	case "$CARGO_TARGET_DIR" in
		/tmp/cargo-target-ldk-node-python-wheels.*)
			rm -rf -- "$CARGO_TARGET_DIR"
			;;
	esac
}
trap cleanup EXIT

echo "Building Python wheel from commit $(git rev-parse HEAD)"
echo "Target: $(uname -s) $(uname -m)"

uv tool run --python 3.11 --from "cibuildwheel[uv]==$CIBUILDWHEEL_VERSION" cibuildwheel \
	bindings/python \
	--config-file bindings/python/pyproject.toml \
	--output-dir "$OUTPUT_DIR"

built_wheels=("$OUTPUT_DIR"/*.whl)
if [[ ${#built_wheels[@]} -ne 1 ]]; then
	echo "Expected exactly one wheel, found ${#built_wheels[@]}." >&2
	exit 1
fi

wheel_path="${built_wheels[0]}"
wheel_name="$(basename "$wheel_path")"
(
	cd "$OUTPUT_DIR"
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$wheel_name"
	else
		shasum -a 256 "$wheel_name"
	fi
) > "$wheel_path.sha256"

echo "Wheel built and tested successfully:"
echo "  $wheel_path"
echo "  $wheel_path.sha256"
