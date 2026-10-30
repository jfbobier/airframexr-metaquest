#!/bin/sh
# Regenerates the committed SPIR-V of the Vulkan staging pass. glslc ships with the Android NDK
# (<ndk>/shader-tools/<host>/glslc). Run from this directory.
set -e
GLSLC=${GLSLC:-glslc}
$GLSLC --target-env=vulkan1.0 -O -o staging_vertex.spv staging_vertex.vert
$GLSLC --target-env=vulkan1.0 -O -o staging_fragment.spv staging_fragment.frag
