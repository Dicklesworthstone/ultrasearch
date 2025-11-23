# Extractous + GraalVM Setup (Windows/Local)

Extractous relies on GraalVM’s Java runtime. The optional `extractous_backend` feature is OFF by default; turn it on only after provisioning GraalVM.

## Install GraalVM CE 23.x
1) Download GraalVM Community Edition 23.x for Windows x64 from Oracle/GraalVM. Example (as of 2025-11-23): `https://download.oracle.com/graalvm/23/latest/graalvm-jdk-23_windows-x64_bin.zip`
2) Unpack or install to a stable path, e.g. `C:\Tools\graalvm-ce-java17-23.1.0`.
3) Set environment variables (PowerShell example):
```
$env:GRAALVM_HOME="C:\Tools\graalvm-ce-java17-23.1.0"
$env:JAVA_HOME=$env:GRAALVM_HOME
$env:PATH="$env:GRAALVM_HOME\bin;$env:PATH"
```
4) Verify:
```
java -version   # should mention GraalVM
```

### Checksum
- Always verify the download against the SHA256 published on the GraalVM download page.
- Example verification (PowerShell):
```
certutil -hashfile .\graalvm-jdk-23_windows-x64_bin.zip SHA256
```
Compare the output to the official hash before installing.

## Build guidance
- The build script for `content-extractor` checks `GRAALVM_HOME`/`JAVA_HOME` when the `extractous_backend` feature is enabled and will error if Java is missing.
- Enable the backend with:
```
cargo build -p content-extractor --features extractous_backend
```
- The index-worker enables Extractous at runtime via `--enable-extractous` flag or `ULTRASEARCH_ENABLE_EXTRACTOUS=1`.

## CI / fallback
- If GraalVM is unavailable on a machine, keep the feature disabled; the SimpleText + Noop stack still builds and runs.
- For CI images, either install GraalVM and export `GRAALVM_HOME`, or build without the feature.

## Notes
- Keep max file size/char limits in config to avoid excessive JVM memory use.
- GraalVM updates are frequent; target CE 23.x per AGENTS.md policy (latest stable).***
