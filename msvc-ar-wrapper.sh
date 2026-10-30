#!/bin/sh
# Wrapper so ring's build script (which passes MSVC lib.exe-style flags for
# the *-pc-windows-msvc target) works with GNU ar from mingw-w64.
# Translates: -out:<path> -nologo <objects...>  ->  crus <path> <objects...>
out=""
inputs=""
for a in "$@"; do
  case "$a" in
    -out:*) out="${a#-out:}" ;;
    -nologo) ;;
    *) inputs="$inputs $a" ;;
  esac
done
# shellcheck disable=SC2086
exec x86_64-w64-mingw32-ar crus "$out" $inputs
