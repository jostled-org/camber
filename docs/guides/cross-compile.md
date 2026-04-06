# Cross-Compilation

Build a static Linux binary from any platform.

## Native Cross-Compile (musl)

Install the target and build:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The binary at `target/x86_64-unknown-linux-musl/release/camber` is fully static — no libc dependency.

### ARM64 (aarch64)

```sh
rustup target add aarch64-unknown-linux-musl
cargo build --release --target aarch64-unknown-linux-musl
```

This requires an ARM64 linker. On macOS, install via:

```sh
brew install messense/macos-cross-toolchains/aarch64-unknown-linux-musl
```

Set the linker in `.cargo/config.toml`:

```toml
[target.aarch64-unknown-linux-musl]
linker = "aarch64-unknown-linux-musl-gcc"
```

## Docker-Based Cross-Compile

No toolchain setup required. Build from the project root:

```sh
docker build -t camber-builder .
docker create --name extract camber-builder
docker cp extract:/usr/local/bin/app ./camber-linux
docker rm extract
```

The resulting binary runs on any Linux system with the same architecture.

## CI Integration

The project's GitHub Actions workflow builds and tests on Linux. To add cross-compilation as a release step:

```yaml
- name: Build Linux binary
  run: |
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl

- name: Upload artifact
  uses: actions/upload-artifact@v4
  with:
    name: camber-linux-amd64
    path: target/x86_64-unknown-linux-musl/release/camber
```

## Verifying the Binary

```sh
file target/x86_64-unknown-linux-musl/release/camber
# camber: ELF 64-bit LSB executable, x86-64, statically linked

ldd target/x86_64-unknown-linux-musl/release/camber
# not a dynamic executable
```
