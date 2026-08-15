#!/bin/sh
# Small release-binary benchmark harness. It intentionally uses only POSIX
# shell utilities so it can run on a developer workstation or a minimal CI
# image without adding benchmark dependencies.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PROJECT_DIR=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
BIN=${FS_BIN:-$PROJECT_DIR/target/release/fs}
TOOLCHAIN=$(awk -F'"' '/^channel[[:space:]]*=/ { print $2; exit }' "$PROJECT_DIR/rust-toolchain.toml")
FILES=${FS_BENCH_FILES:-1000}
PAYLOAD=${FS_BENCH_PAYLOAD:-4096}
# Keep the default fixture beneath the checkout. The tool deliberately rejects
# intermediate symlinks, and macOS commonly exposes TMPDIR through `/var` as a
# symlink to `/private/var`; a relative checkout path keeps this harness valid
# under the same root-establishment contract it measures.
WORK_ROOT=${FS_BENCH_DIR:-$PROJECT_DIR/.fs-bench-$$}

case "$FILES" in
    ''|*[!0-9]*) echo "FS_BENCH_FILES must be a non-negative integer" >&2; exit 2 ;;
esac
case "$PAYLOAD" in
    ''|*[!0-9]*) echo "FS_BENCH_PAYLOAD must be a non-negative integer" >&2; exit 2 ;;
esac

# Never make a user-supplied path removable by accident. The harness owns its
# generated directory for this invocation and refuses to reuse an existing
# path.
if [ -e "$WORK_ROOT" ]; then
    echo "benchmark directory already exists: $WORK_ROOT" >&2
    exit 2
fi

cleanup() {
    rm -rf "$WORK_ROOT"
}
trap cleanup EXIT HUP INT TERM

if [ ! -x "$BIN" ]; then
    echo "building release binary: $BIN" >&2
    rustup run "$TOOLCHAIN" cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"
fi

SOURCE=$WORK_ROOT/source
DESTINATION=$WORK_ROOT/destination
mkdir -p "$SOURCE"

echo "generating $FILES files of approximately $PAYLOAD bytes"
i=0
while [ "$i" -lt "$FILES" ]; do
    # printf is used instead of a temporary-name scheme derived from the
    # destination filename. The benchmark is also useful for NAME_MAX-safe
    # publication because the tool owns its bounded temporary namespace.
    file="$SOURCE/file-$i"
    : > "$file"
    if [ "$PAYLOAD" -gt 0 ]; then
        # Repeat a short deterministic line and truncate to the requested
        # payload size. dd is part of the POSIX utility set on supported hosts.
        printf 'fs-bench-%s\n' "$i" | dd of="$file" bs=1 count="$PAYLOAD" conv=sync 2>/dev/null
    fi
    i=$((i + 1))
done

timed() {
    label=$1
    shift
    start=$(date +%s)
    "$@"
    end=$(date +%s)
    elapsed=$((end - start))
    printf '%-18s %ss\n' "$label" "$elapsed"
}

echo "binary: $BIN"
timed "cp (populate)" "$BIN" cp --no-progress "$SOURCE" "$DESTINATION"
timed "sync (metadata)" "$BIN" sync --no-progress "$SOURCE" "$DESTINATION"
timed "sync (hash)" "$BIN" sync --no-progress --check=hash "$SOURCE" "$DESTINATION"

if [ "$FILES" -gt 0 ]; then
    printf 'changed\n' >> "$SOURCE/file-0"
    timed "cp (one change)" "$BIN" cp --no-progress "$SOURCE" "$DESTINATION"
fi

count=$(find "$DESTINATION" -type f -print | wc -l | tr -d '[:space:]')
if [ "$count" -ne "$FILES" ]; then
    echo "benchmark verification failed: expected $FILES files, found $count" >&2
    exit 1
fi
echo "verified: $count files converged"
