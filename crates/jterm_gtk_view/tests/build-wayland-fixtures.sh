#!/usr/bin/env bash
set -euo pipefail

if (($# != 2)); then
    printf 'Usage: %s WLROOTS_PROTOCOL_DIRECTORY OUTPUT_DIRECTORY\n' "$0" >&2
    exit 2
fi

fixtures=$(cd -- "$(dirname -- "$0")" && pwd)
protocols=$1
output=$2
mkdir -p -- "$output"
read -r -a wayland_flags <<< "$(pkg-config --cflags --libs wayland-client)"

build_client() {
    local source=$1 protocol=$2 binary=$3
    local header="$output/$protocol-client-protocol.h"
    local code="$output/$protocol-protocol.c"
    wayland-scanner client-header "$protocols/$protocol.xml" "$header"
    wayland-scanner private-code "$protocols/$protocol.xml" "$code"
    "${CC:-cc}" -std=c11 -Wall -Wextra -Werror -I "$output" \
        "$fixtures/$source" "$code" -o "$output/$binary" "${wayland_flags[@]}"
}

build_client wayland_input_method.c input-method-unstable-v2 native-ime-client
build_client wayland_pointer.c wlr-virtual-pointer-unstable-v1 native-pointer
