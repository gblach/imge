name := `cargo pkgid | sed -E 's/.*#([^@]+)@.*/\1/;t;s|.*/([^/#]+)#.*|\1|'`
version := `cargo pkgid | sed -E 's/.*#//; s/.*@//'`
targets := "aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu"

release:
    #!/usr/bin/env bash
    set -euo pipefail
    for target in {{ targets }}; do
        # Without it lzma-sys links this machine's liblzma.so for the native target, which needs a
        # newer glibc than zig links against.
        LZMA_API_STATIC=1 cargo zigbuild --release --target "$target"
        # cargo hard-links the binary to its own cache, so moving it would leave the output sharing
        # that file with the next build; a fresh copy keeps them apart.
        out="{{ name }}-{{ version }}-$target"
        rm -f "$out"
        cp "target/$target/release/{{ name }}" "$out"
    done
