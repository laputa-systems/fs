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
LARGE_MB=${FS_BENCH_LARGE_MB:-64}
MIXED_DIRS=${FS_BENCH_MIXED_DIRS:-32}
MIXED_FILES_PER_DIR=${FS_BENCH_MIXED_FILES_PER_DIR:-16}
STARTUP_RUNS=${FS_BENCH_STARTUP_RUNS:-1000}
# Keep the default fixture beneath the checkout. The tool deliberately rejects
# intermediate symlinks, and macOS commonly exposes TMPDIR through `/var` as a
# symlink to `/private/var`; a relative checkout path keeps this harness valid
# under the same root-establishment contract it measures.
WORK_ROOT=${FS_BENCH_DIR:-$PROJECT_DIR/.work/tmp/fs-bench-$$}

check_nonnegative_integer() {
    name=$1
    value=$2
    case "$value" in
        ''|*[!0-9]*) echo "$name must be a non-negative integer" >&2; exit 2 ;;
    esac
}

check_positive_integer() {
    name=$1
    value=$2
    check_nonnegative_integer "$name" "$value"
    if [ "$value" -eq 0 ]; then
        echo "$name must be greater than zero" >&2
        exit 2
    fi
}

check_nonnegative_integer FS_BENCH_FILES "$FILES"
check_nonnegative_integer FS_BENCH_PAYLOAD "$PAYLOAD"
check_positive_integer FS_BENCH_LARGE_MB "$LARGE_MB"
check_nonnegative_integer FS_BENCH_MIXED_DIRS "$MIXED_DIRS"
check_nonnegative_integer FS_BENCH_MIXED_FILES_PER_DIR "$MIXED_FILES_PER_DIR"
check_positive_integer FS_BENCH_STARTUP_RUNS "$STARTUP_RUNS"

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

timed() {
    label=$1
    shift
    start=$(date +%s)
    "$@"
    end=$(date +%s)
    elapsed=$((end - start))
    printf '%-24s %ss\n' "$label" "$elapsed"
}

timed_repeat() {
    label=$1
    runs=$2
    shift 2
    start=$(date +%s)
    i=0
    while [ "$i" -lt "$runs" ]; do
        "$@" >/dev/null
        i=$((i + 1))
    done
    end=$(date +%s)
    elapsed=$((end - start))
    printf '%-24s %ss total (%s runs)\n' "$label" "$elapsed" "$runs"
}

run_fs() (
    cd "$WORK_ROOT"
    "$BIN" "$@"
)

write_small_payload() {
    file=$1
    token=$2
    : > "$file"
    if [ "$PAYLOAD" -gt 0 ]; then
        printf 'fs-bench-%s\n' "$token" | dd of="$file" bs=1 count="$PAYLOAD" conv=sync 2>/dev/null
    fi
}

echo "binary: $BIN"
timed_repeat "startup (--version)" "$STARTUP_RUNS" "$BIN" --version

SOURCE_PATH=$WORK_ROOT/source
DESTINATION_PATH=$WORK_ROOT/destination
mkdir -p "$SOURCE_PATH"
echo "generating $FILES files of approximately $PAYLOAD bytes"
i=0
while [ "$i" -lt "$FILES" ]; do
    # The generated names remain independent of publication's temporary-name
    # scheme, so the workload also exercises NAME_MAX-safe publication.
    write_small_payload "$SOURCE_PATH/file-$i" "$i"
    i=$((i + 1))
done

timed "cp (small, no progress)" run_fs cp --no-progress source destination
timed "sync (metadata no-op)" run_fs sync --no-progress source destination
timed "sync (hash no-op)" run_fs sync --no-progress --check=hash source destination

if [ -t 2 ]; then
    PROGRESS_DESTINATION=$WORK_ROOT/progress-destination
    timed "cp (small, progress)" run_fs cp source progress-destination
    progress_count=$(find "$PROGRESS_DESTINATION" -type f -print | wc -l | tr -d '[:space:]')
    if [ "$progress_count" -ne "$FILES" ]; then
        echo "benchmark verification failed: expected $FILES progress files, found $progress_count" >&2
        exit 1
    fi
    echo "verified: $progress_count files copied with terminal progress enabled"
else
    echo "progress overhead skipped: stderr is not a terminal"
fi

if [ "$FILES" -gt 0 ]; then
    printf 'changed\n' >> "$SOURCE_PATH/file-0"
    timed "cp (one change)" run_fs cp --no-progress source destination
fi

count=$(find "$DESTINATION_PATH" -type f -print | wc -l | tr -d '[:space:]')
if [ "$count" -ne "$FILES" ]; then
    echo "benchmark verification failed: expected $FILES files, found $count" >&2
    exit 1
fi
echo "verified: $count small files converged"

LARGE_SOURCE=$WORK_ROOT/large/source
LARGE_DESTINATION=$WORK_ROOT/large/destination
mkdir -p "$LARGE_SOURCE"
dd if=/dev/zero of="$LARGE_SOURCE/payload" bs=1048576 count="$LARGE_MB" 2>/dev/null
timed "cp (large file)" run_fs cp --no-progress large/source/payload large/destination
if ! cmp "$LARGE_SOURCE/payload" "$LARGE_DESTINATION" >/dev/null 2>&1; then
    echo "benchmark verification failed: large file differs after copy" >&2
    exit 1
fi
echo "verified: $LARGE_MB MiB large file copied"

MIXED_SOURCE=$WORK_ROOT/mixed/source
MIXED_DESTINATION=$WORK_ROOT/mixed/destination
mkdir -p "$MIXED_SOURCE"
directory=0
while [ "$directory" -lt "$MIXED_DIRS" ]; do
    child=$MIXED_SOURCE/directory-$directory
    mkdir "$child"
    file_index=0
    while [ "$file_index" -lt "$MIXED_FILES_PER_DIR" ]; do
        write_small_payload "$child/file-$file_index" "$directory-$file_index"
        file_index=$((file_index + 1))
    done
    directory=$((directory + 1))
done
timed "cp (mixed tree)" run_fs cp --no-progress mixed/source mixed/destination
mixed_expected=$((MIXED_DIRS * MIXED_FILES_PER_DIR))
mixed_count=$(find "$MIXED_DESTINATION" -type f -print | wc -l | tr -d '[:space:]')
if [ "$mixed_count" -ne "$mixed_expected" ]; then
    echo "benchmark verification failed: expected $mixed_expected mixed files, found $mixed_count" >&2
    exit 1
fi
echo "verified: $mixed_count files across $MIXED_DIRS directories"
