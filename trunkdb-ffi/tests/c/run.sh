#!/bin/sh
# Builds the library and runs every_function.c against it, under
# AddressSanitizer (SPEC §179). From anywhere in the repository:
#
#   sh trunkdb-ffi/tests/c/run.sh
set -eu
root=$(cd "$(dirname "$0")/../../.." && pwd)
cargo build -q -p trunkdb-ffi --manifest-path "$root/Cargo.toml"
lib="$root/target/debug"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
${CC:-cc} -std=c99 -Wall -Wextra -Werror -fsanitize=address -g \
    -I "$root/trunkdb-ffi/include" "$root/trunkdb-ffi/tests/c/every_function.c" \
    -L "$lib" -ltrunkdb -Wl,-rpath,"$lib" -o "$work/every_function"
"$work/every_function" "$work"
