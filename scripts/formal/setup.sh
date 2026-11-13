#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends ca-certificates curl unzip python3 build-essential pkg-config libssl-dev
case "$(uname -m)" in
  aarch64)
    target=aarch64-unknown-linux-gnu
    rustup_sha=e3853c5a252fca15252d07cb23a1bdd9377a8c6f3efa01531109281ae47f841c
    kani_sha=0042d2688bf1550270def4a79c96aaeea138631818cb4ab5480120f2d51a942c
    verus_arch=arm64
    verus_sha=bc28850dd5efd1679a4cf4ffd3309921c3523c69fa4757079e5d12d4122d400d
    ;;
  x86_64)
    target=x86_64-unknown-linux-gnu
    rustup_sha=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c
    kani_sha=32e2b484d73ede0bbf64a2cf0879c4259422497d8aec4ee67d448de9ae7843d3
    verus_arch=x86
    verus_sha=f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c
    ;;
  *) echo 'Unsupported formal-verifier architecture' >&2; exit 125 ;;
esac
fetch() {
  curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --connect-timeout 15 --max-time 180 "$1" --output "$2"
  echo "$3  $2" | sha256sum --check --strict
}
fetch "https://static.rust-lang.org/rustup/archive/1.28.2/$target/rustup-init" /tmp/rustup-init "$rustup_sha"
chmod +x /tmp/rustup-init
/tmp/rustup-init -y --profile minimal --default-toolchain 1.99.1 --no-modify-path
export PATH="/root/.cargo/bin:$PATH"
rustup set auto-self-update disable
rustup component add --toolchain 1.99.1 clippy rustfmt
export CARGO_BUILD_JOBS=1
cargo install --locked kani-verifier --version 0.68.0
fetch "https://github.com/model-checking/kani/releases/download/kani-0.68.0/kani-0.68.0-$target.tar.gz" /tmp/kani.tar.gz "$kani_sha"
cargo kani setup --use-local-bundle /tmp/kani.tar.gz
fetch "https://github.com/verus-lang/verus/releases/download/release/0.2026.11.02.8bf68ee/verus-0.2026.11.02.8bf68ee-$verus_arch-linux.zip" /tmp/verus.zip "$verus_sha"
unzip -q /tmp/verus.zip -d /opt
ln -s "/opt/verus-$verus_arch-linux/verus" /usr/local/bin/verus
kani --version | grep -Fx 'Kani Rust Verifier 0.68.0 (standalone)'
verus --version | grep -F '0.2026.11.02.8bf68ee'
rm -rf /var/lib/apt/lists/* /root/.cargo/registry /tmp/rustup-init /tmp/kani.tar.gz /tmp/verus.zip
# Shuttle executes offline. Both its manifest and lockfile participate in the
# immutable image identity, so a dependency change requires bounded setup again.
cargo fetch --locked --manifest-path /source/verification/shuttle/Cargo.toml
