#!/usr/bin/env bash
set -euo pipefail
(($#)) || { echo "Usage: $0 ELF ..." >&2; exit 2; }

for bin in "$@"; do
    symbols=$(objdump -T -- "$bin")
    versions=$(readelf -V -- "$bin")
    printf '%s\n' "$symbols" "$versions" | awk -v bin="$bin" '
        {
            while (match($0, /GLIBC_[0-9]+(\.[0-9]+)*/)) {
                version = substr($0, RSTART, RLENGTH)
                $0 = substr($0, RSTART + RLENGTH)
                split(version, parts, /[_.]/)
                if ((parts[2] + 0 > 2 || (parts[2] == 2 && parts[3] + 0 > 36)) && !seen[version]++) {
                    print bin ": unsupported " version
                    bad = 1
                }
            }
        }
        END { exit bad }
    ' >&2
done
