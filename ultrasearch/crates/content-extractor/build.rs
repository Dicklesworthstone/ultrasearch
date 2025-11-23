// Build-time guard for the optional `extractous_backend` feature.
// We only enforce requirements when the feature is enabled to keep
// the default lightweight build working everywhere.

#[cfg(feature = "extractous_backend")]
fn main() {
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    println!("cargo:rerun-if-env-changed=GRAALVM_HOME");
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    println!("cargo:rerun-if-env-changed=PATH");

    let graal_home = env::var("GRAALVM_HOME").ok();
    let java_home = env::var("JAVA_HOME").ok();

    let java_path: PathBuf = graal_home
        .as_ref()
        .or(java_home.as_ref())
        .map(|home| {
            let mut p = PathBuf::from(home);
            p.push("bin");
            p.push(if cfg!(windows) { "java.exe" } else { "java" });
            p
        })
        .unwrap_or_else(|| PathBuf::from("java"));

    // Try to run `java -version` to verify availability and that we're on GraalVM.
    let version_output = Command::new(&java_path).arg("-version").output().ok();

    let mut is_graal = false;
    if let Some(out) = &version_output {
        let combined = String::from_utf8_lossy(&out.stderr).to_string()
            + &String::from_utf8_lossy(&out.stdout);
        is_graal = combined.to_ascii_lowercase().contains("graalvm");
        if !is_graal {
            panic!(
                "extractous_backend requires GraalVM; found Java at {} but version output did not contain 'GraalVM'. First line: {}",
                java_path.display(),
                combined.lines().next().unwrap_or("")
            );
        }
    } else {
        println!(
            "cargo:warning=extractous_backend enabled but `java` not found at {}. Set GRAALVM_HOME or JAVA_HOME to a GraalVM CE 23.x install.",
            java_path.display()
        );
    }

    if version_output.is_none() || !is_graal {
        panic!(
            "extractous_backend requires GraalVM JDK available via GRAALVM_HOME or JAVA_HOME. Install GraalVM CE 23.x and set one of these env vars."
        );
    }
}

#[cfg(not(feature = "extractous_backend"))]
fn main() {
    // Keep build script inert when the heavy backend is disabled.
    println!("cargo:rerun-if-changed=build.rs");
}
