# Gate toolchain (tools/container.sh): Rust 1.93.1 by digest, rustfmt and clippy from the same
# channel manifest (rustup checks their hashes), cargo-fuzz and cargo-deny built from crates.io with
# their own Cargo.lock (--locked, checksums verified by cargo); CMake, Ninja, GoogleTest and clang
# (libFuzzer) for the C++ query language from a dated Debian snapshot at exact versions (apt
# verifies the signed Release files).
FROM rust:1.93.1-trixie@sha256:ecbe59a8408895edd02d9ef422504b8501dd9fa1526de27a45b73406d734d659
ARG SNAPSHOT=20261101T000000Z
RUN rm -f /etc/apt/sources.list.d/* \
 && printf 'deb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s trixie main\ndeb [check-valid-until=no] https://snapshot.debian.org/archive/debian/%s trixie-updates main\ndeb [check-valid-until=no] https://snapshot.debian.org/archive/debian-security/%s trixie-security main\n' \
      "$SNAPSHOT" "$SNAPSHOT" "$SNAPSHOT" > /etc/apt/sources.list \
 && apt-get update -qq \
 && apt-get install -y -qq --no-install-recommends cmake=3.31.6-2 ninja-build=1.12.1-1 libgtest-dev=1.16.0-1 \
      clang-19=1:19.1.7-3+b1 libclang-rt-19-dev=1:19.1.7-3+b1 >/dev/null \
 && rm -rf /var/lib/apt/lists/* \
 && rustup component add rustfmt clippy \
 && cargo install --locked --root /usr/local cargo-fuzz@0.13.2 cargo-deny@0.20.2 \
 && rm -rf /usr/local/cargo/registry /usr/local/cargo/git
