//! Artifact building: cross-compiles the Rust binaries (cargo-lambda) and
//! bundles the Node functions (esbuild via gradle codegen), then zips each into
//! a deployable artifact under `dist/`.
//!
//! Only the unique artifacts are built (deduped across memory configs, and
//! across architecture for the arch-independent Node bundles).

use crate::config::{Arch, ArtifactKey, Jitter, Lang, Scenario};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A built, ready-to-deploy artifact.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub key: ArtifactKey,
    /// Path to the deployable zip.
    pub zip_path: PathBuf,
    /// Compressed deployment package size (what gets downloaded into the sandbox).
    pub zip_size_bytes: u64,
    /// Uncompressed size of the deployed code (raw bootstrap binary, or the JS
    /// bundle plus any sidecar files). This is closer to what is loaded/linked
    /// during a cold start than the zip size.
    pub unzipped_size_bytes: u64,
}

/// Reconstructs the artifact map from zips already present in `dist/`, without
/// building. Used by `run --skip-build`. The zip path is deterministic per
/// `ArtifactKey` (`dist/{label}.zip`); the compressed size comes from the file
/// on disk and the unzipped size is summed from the zip's central directory
/// (each entry records its uncompressed size, so this needs no inflation).
/// Fails loud if any expected artifact is missing, so a stale/partial `dist/`
/// cannot silently run the wrong code.
pub fn load_artifacts_from_dist(
    repo_root: &Path,
    keys: &[ArtifactKey],
) -> Result<(BTreeMap<String, Artifact>, serde_json::Value)> {
    let dist = repo_root.join("dist");
    let mut artifacts = BTreeMap::new();
    for key in keys {
        let label = key.label();
        let zip_path = dist.join(format!("{label}.zip"));
        let meta = std::fs::metadata(&zip_path).with_context(|| {
            format!(
                "artifact {} missing at {} (run `build` first, or drop --skip-build)",
                label,
                zip_path.display()
            )
        })?;
        let unzipped_size_bytes = unzipped_size_from_zip(&zip_path)
            .with_context(|| format!("reading unzipped size of {}", zip_path.display()))?;
        artifacts.insert(
            label.clone(),
            Artifact {
                key: *key,
                zip_path,
                zip_size_bytes: meta.len(),
                unzipped_size_bytes,
            },
        );
    }
    let manifest = json!({
        "tools": { "note": "skip-build: reused existing dist/ artifacts, tool versions not captured" },
        "artifacts": artifacts_manifest(&artifacts),
    });
    Ok((artifacts, manifest))
}

/// Builds the `artifacts` array of the build manifest: one JSON object per
/// artifact with its label, zip path, and sizes. Shared by `build_all` and
/// `load_artifacts_from_dist` so the manifest shape is identical regardless of
/// whether the artifacts were freshly built or reused from `dist/`.
fn artifacts_manifest(artifacts: &BTreeMap<String, Artifact>) -> serde_json::Value {
    json!(
        artifacts
            .iter()
            .map(|(label, a)| {
                json!({
                    "label": label,
                    "zip": a.zip_path.display().to_string(),
                    "zip_size_bytes": a.zip_size_bytes,
                    "unzipped_size_bytes": a.unzipped_size_bytes,
                })
            })
            .collect::<Vec<_>>()
    )
}

/// Builds all unique artifacts for the given keys. Returns them keyed for
/// fan-out to functions, plus a JSON manifest of what was built.
pub fn build_all(
    repo_root: &Path,
    keys: &[ArtifactKey],
) -> Result<(BTreeMap<String, Artifact>, serde_json::Value)> {
    let dist = repo_root.join("dist");
    std::fs::create_dir_all(&dist).context("creating dist/")?;

    // Group work: Rust keys build individually; Node keys share one bundle pass;
    // Java keys share one gradle invocation.
    let mut rust_keys = Vec::new();
    let mut node_keys = Vec::new();
    let mut java_keys = Vec::new();
    let mut python_keys = Vec::new();
    let mut go_keys = Vec::new();
    for key in keys {
        match key.lang {
            Lang::Rust => rust_keys.push(*key),
            Lang::Node => node_keys.push(*key),
            Lang::Java => java_keys.push(*key),
            Lang::Python => python_keys.push(*key),
            Lang::Go => go_keys.push(*key),
        }
    }

    let mut artifacts = BTreeMap::new();

    // --- Rust: cross-compile each (scenario, arch, opt) via cargo-lambda. ---
    for key in &rust_keys {
        let art = build_rust(repo_root, &dist, key)?;
        artifacts.insert(key.label(), art);
    }

    // --- Node: gradle codegen + npm install + esbuild bundle (once), then zip
    //     each requested scenario artifact. ---
    if !node_keys.is_empty() {
        node_codegen_and_bundle(repo_root)?;
        for key in &node_keys {
            let art = zip_node(repo_root, &dist, key)?;
            artifacts.insert(key.label(), art);
        }
    }

    // --- Java: one gradle build of all requested scenario subprojects'
    //     `buildZip` tasks, then copy each distribution zip into dist/. ---
    if !java_keys.is_empty() {
        let arts = build_java(repo_root, &dist, &java_keys)?;
        artifacts.extend(arts);
    }

    // --- Python: assemble each scenario bundle (handler + pip-installed deps
    //     for the Lambda target arch), then zip. ---
    for key in &python_keys {
        let art = build_python(repo_root, &dist, key)?;
        artifacts.insert(key.label(), art);
    }

    // --- Go: cross-compile each (scenario, arch) to a `bootstrap` executable,
    //     then zip. ---
    for key in &go_keys {
        let art = build_go(repo_root, &dist, key)?;
        artifacts.insert(key.label(), art);
    }

    // Build manifest: tool versions + artifact sizes for reproducibility.
    let manifest = json!({
        "tools": {
            // A tool that can't be queried records "<unavailable>" rather than an
            // empty string, so the reproducibility manifest is unambiguous.
            "rustc": tool_version(Command::new("rustc").arg("--version")),
            "cargo_lambda": tool_version(Command::new("cargo").args(["lambda", "--version"])),
            "node": tool_version(Command::new("node").arg("--version")),
            "python": tool_version(Command::new("python3").arg("--version")),
            "pip": tool_version(Command::new("python3").args(["-m", "pip", "--version"])),
            "go": tool_version(Command::new("go").arg("version")),
            // `java --version` (double dash) prints to stdout on JDK 9+, unlike
            // the legacy `-version` which goes to stderr.
            "java": tool_version(Command::new("java").arg("--version")),
        },
        "artifacts": artifacts_manifest(&artifacts),
    });

    Ok((artifacts, manifest))
}

/// Cross-compiles one Rust scenario for one architecture and zips the bootstrap.
fn build_rust(repo_root: &Path, dist: &Path, key: &ArtifactKey) -> Result<Artifact> {
    let arch_flag = match key.arch {
        Arch::Arm64 => "--arm64",
        Arch::X86_64 => "--x86-64",
    };

    // The hello/oneclient/threeclient crates are workspace members; the smithy
    // and smithyfull crates are standalone cargo projects (they depend on
    // gradle-generated code). smithyfull reuses the SSDK generated under the
    // smithy project, so it also needs that gradle codegen to have run.
    let (workdir, package): (PathBuf, Option<&str>) = match key.scenario {
        Scenario::Hello => (repo_root.to_path_buf(), Some("lambdabench-rust-hello")),
        Scenario::OneClient => (repo_root.to_path_buf(), Some("lambdabench-rust-oneclient")),
        Scenario::ThreeClient => (
            repo_root.to_path_buf(),
            Some("lambdabench-rust-threeclient"),
        ),
        Scenario::LetterCount => (
            repo_root.to_path_buf(),
            Some("lambdabench-rust-lettercount"),
        ),
        Scenario::Authz => (repo_root.to_path_buf(), Some("lambdabench-rust-authz")),
        Scenario::Batch => (repo_root.to_path_buf(), Some("lambdabench-rust-batch")),
        Scenario::Cache => (repo_root.to_path_buf(), Some("lambdabench-rust-cache")),
        Scenario::Smithy => {
            // Ensure server SDK is generated before building.
            gradle_codegen(&repo_root.join("scenarios/rust/smithy"), ":server:build")?;
            (repo_root.join("scenarios/rust/smithy/server"), None)
        }
        Scenario::SmithyFull => {
            // Reuses the SSDK generated under the smithy project.
            gradle_codegen(&repo_root.join("scenarios/rust/smithy"), ":server:build")?;
            (repo_root.join("scenarios/rust/smithyfull"), None)
        }
    };

    let opt = key.opt.unwrap_or(crate::config::Opt::O3);
    // Every Rust artifact key carries a Jitter; `all_cells` populates it for
    // every Rust cell. Make the invariant explicit: a Rust build with no
    // jitter dimension is a bug, not a default to silently fill in.
    let jitter = key
        .jitter
        .expect("rust artifact key must carry a Jitter dimension");

    // Distinct target dir per (opt-level, jitter). The jitter split is required
    // for correctness: `AWS_LC_SYS_NO_JITTER_ENTROPY` does NOT key cargo's output
    // path (only `rerun-if-env-changed`), so sharing a dir across jitter settings
    // would let a rebuild for one clobber the other's `aws-lc-sys` objects and ship
    // the wrong build. opt-level IS in cargo's fingerprint, so it needs no separate
    // dir for correctness; keeping one per opt-level anyway avoids churning a shared
    // cache on every opt switch, for fast incremental rebuilds.
    let dir_tag = match jitter {
        Jitter::Off => opt.as_str().to_string(),
        Jitter::On => format!("{}-jitter", opt.as_str()),
    };
    let target_dir = match key.scenario {
        Scenario::Smithy => {
            repo_root.join(format!("scenarios/rust/smithy/server/target-{dir_tag}"))
        }
        Scenario::SmithyFull => {
            repo_root.join(format!("scenarios/rust/smithyfull/target-{dir_tag}"))
        }
        _ => repo_root.join(format!("target-{dir_tag}")),
    };

    // Unique output dir per (scenario, arch, opt): cargo-lambda writes every build
    // to `<lambda-dir>/bootstrap/bootstrap` by default, so a shared dir would let a
    // later build overwrite an earlier one and we could zip the wrong binary.
    // `--flatten bootstrap` drops the extra subdir, putting the binary directly at
    // `<lambda-dir>/bootstrap`.
    let lambda_dir = target_dir.join(format!("out-{}", key.label()));
    let _ = std::fs::remove_dir_all(&lambda_dir);

    let mut cmd = Command::new("cargo");
    cmd.current_dir(&workdir)
        .args(["lambda", "build", "--profile", "lambda", arch_flag])
        .arg("--lambda-dir")
        .arg(&lambda_dir)
        .args(["--flatten", "bootstrap"]);
    if let Some(pkg) = package {
        cmd.args(["-p", pkg]);
    }
    // The default (Jitter::Off) build sets the flag; the Jitter::On A/B variant
    // does not. See `config::Jitter` for what it does and why it is the default.
    if matches!(jitter, Jitter::Off) {
        cmd.env("AWS_LC_SYS_NO_JITTER_ENTROPY", "1");
    }
    // opt-level is a benchmark dimension. Override the `lambda` profile's opt-level
    // via cargo's env var so we don't edit Cargo.toml per build. All other cold-start
    // optimizations (fat LTO, codegen-units=1, strip, panic=abort) are unchanged. The
    // env key must name the profile in use (`lambda`), not `release`, or it silently
    // no-ops and the opt-level dimension stops taking effect.
    cmd.env("CARGO_PROFILE_LAMBDA_OPT_LEVEL", opt.cargo_opt_level());
    cmd.env("CARGO_TARGET_DIR", &target_dir);
    run(cmd, &format!("cargo-lambda build {}", key.label()))?;

    // With `--flatten bootstrap`, the binary is at `<lambda-dir>/bootstrap`.
    let bootstrap = lambda_dir.join("bootstrap");
    if !bootstrap.is_file() {
        bail!("expected bootstrap at {}", bootstrap.display());
    }
    let unzipped_size_bytes = std::fs::metadata(&bootstrap)?.len();
    let zip_path = dist.join(format!("{}.zip", key.label()));
    zip_single(&bootstrap, "bootstrap", &zip_path, 0o755)?;
    let zip_size_bytes = std::fs::metadata(&zip_path)?.len();
    Ok(Artifact {
        key: *key,
        zip_path,
        zip_size_bytes,
        unzipped_size_bytes,
    })
}

/// Runs gradle codegen + npm install + esbuild bundle for the Node artifacts.
fn node_codegen_and_bundle(repo_root: &Path) -> Result<()> {
    let node_root = repo_root.join("scenarios/node");

    // 1. Smithy TS SSDK codegen. Gradle tracks its output dir for incremental
    //    builds, so we must not add a node_modules tree there (it would fail
    //    snapshotting the deep nested paths next run). Copy the generated SSDK to a
    //    sibling dir gradle does not track, and build it there.
    let gradle_proj = node_root.join("smithy");
    gradle_codegen(&gradle_proj, ":smithy:build")?;

    let generated =
        gradle_proj.join("smithy/build/smithyprojections/smithy/source/typescript-ssdk-codegen");
    let ssdk = node_root.join(".ssdk"); // outside gradle's tracked output
    copy_ssdk(&generated, &ssdk)?;

    // 2. Build the generated SSDK (tsc) so it can be consumed via the file: dep.
    //    Run the three tsc targets directly rather than the SSDK's
    //    `concurrently`-based `build` script to avoid depending on that dev
    //    binary being present.
    run(cmd_in(&ssdk, "yarn", &["install"]), "ssdk yarn install")?;
    for target in ["build:cjs", "build:es", "build:types"] {
        run(
            cmd_in(&ssdk, "yarn", &["run", target]),
            &format!("ssdk {target}"),
        )?;
    }

    // 3. Install the Node artifacts package and run esbuild. Remove a possibly-stale
    //    node_modules so the file: dep symlink under it is recreated cleanly (an
    //    interrupted install can leave a stale symlink that fails the next one with
    //    EEXIST). `npm ci` installs strictly from the committed lockfile, so the
    //    artifacts stay reproducible and the lockfile is never rewritten mid-build.
    let _ = std::fs::remove_dir_all(node_root.join("node_modules"));
    run(cmd_in(&node_root, "npm", &["ci"]), "node npm ci")?;
    run(cmd_in(&node_root, "node", &["build.mjs"]), "node esbuild")?;
    Ok(())
}

/// Copies the freshly generated SSDK sources to a build dir outside gradle's
/// tracked output, replacing any previous copy. Excludes build artifacts so the
/// copy is just the codegen sources + package manifests.
fn copy_ssdk(src: &Path, dst: &Path) -> Result<()> {
    if !src.join("package.json").exists() {
        bail!("generated SSDK not found at {}", src.display());
    }
    let _ = std::fs::remove_dir_all(dst);
    std::fs::create_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    // Use rsync for a fast, correct recursive copy, excluding any stray build
    // output that might exist in the source tree.
    let status = Command::new("rsync")
        .args(["-a", "--delete"])
        .args(["--exclude", "node_modules", "--exclude", "dist-*"])
        .arg(format!("{}/", src.display()))
        .arg(format!("{}/", dst.display()))
        .status()
        .context("spawning rsync to copy SSDK")?;
    if !status.success() {
        bail!("rsync of SSDK failed with status {status}");
    }
    Ok(())
}

/// Zips a Node scenario bundle directory (index.mjs plus any sidecar files such
/// as re2.wasm) into a deployable artifact.
fn zip_node(repo_root: &Path, dist: &Path, key: &ArtifactKey) -> Result<Artifact> {
    let bundle_dir = repo_root
        .join("scenarios/node/dist")
        .join(key.scenario.as_str());
    if !bundle_dir.join("index.mjs").exists() {
        bail!("missing node bundle at {}", bundle_dir.display());
    }

    let unzipped_size_bytes = walk(&bundle_dir)?
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let zip_path = dist.join(format!("{}.zip", key.label()));
    zip_dir(&bundle_dir, &zip_path)?;
    let zip_size_bytes = std::fs::metadata(&zip_path)?.len();
    Ok(Artifact {
        key: *key,
        zip_path,
        zip_size_bytes,
        unzipped_size_bytes,
    })
}

/// Builds the requested Java scenario artifacts. Runs a single gradle
/// invocation over the `scenarios/java` multi-project build, executing each
/// requested scenario subproject's `buildZip` task (which produces a Lambda
/// deployment zip under `<subproject>/build/distributions/`), then copies each
/// zip into `dist/` under its artifact label. The smithy/smithyfull subprojects
/// run their own `java-codegen` server SDK generation as part of their build
/// graph, so no separate codegen step is needed here.
fn build_java(
    repo_root: &Path,
    dist: &Path,
    keys: &[ArtifactKey],
) -> Result<BTreeMap<String, Artifact>> {
    let java_root = repo_root.join("scenarios/java");

    // One gradle call building every requested scenario's buildZip task, e.g.
    // `:hello:buildZip :oneclient:buildZip`. Gradle resolves the shared codegen
    // dependencies once across the build. The Gradle toolchain pins the bytecode
    // to the `java25` Lambda runtime regardless of the JDK gradle itself runs on.
    let tasks: Vec<String> = keys
        .iter()
        .map(|key| format!(":{}:buildZip", key.scenario.as_str()))
        .collect();
    run_gradle(&java_root, &tasks, "gradle java buildZip")?;

    // Each subproject's buildZip writes exactly one zip into its
    // build/distributions/ dir. Copy it to dist/<label>.zip and measure.
    let mut artifacts = BTreeMap::new();
    for key in keys {
        let scenario = key.scenario.as_str();
        let dist_dir = java_root.join(scenario).join("build/distributions");
        let built_zip = single_zip_in(&dist_dir).with_context(|| {
            format!(
                "locating gradle distribution zip for java scenario {scenario} in {}",
                dist_dir.display()
            )
        })?;
        let zip_path = dist.join(format!("{}.zip", key.label()));
        std::fs::copy(&built_zip, &zip_path).with_context(|| {
            format!("copying {} to {}", built_zip.display(), zip_path.display())
        })?;
        let zip_size_bytes = std::fs::metadata(&zip_path)?.len();
        let unzipped_size_bytes = unzipped_size_from_zip(&zip_path)
            .with_context(|| format!("reading unzipped size of {}", zip_path.display()))?;
        artifacts.insert(
            key.label(),
            Artifact {
                key: *key,
                zip_path,
                zip_size_bytes,
                unzipped_size_bytes,
            },
        );
    }
    Ok(artifacts)
}

/// Assembles one Python scenario bundle and zips it for deployment: `handler.py`
/// (plus, for `authz`, the generated public JWK) staged flat at the zip root, then
/// `pip install --target` for any declared `requirements.txt`. Deps resolve for the
/// *Lambda* target (`python3.14`, the cell's arch, manylinux), not the host, so it
/// cross-builds on macOS: boto3/PyJWT are pure-Python (arch-independent wheels) and
/// `cryptography` ships a per-arch native `abi3` wheel. Per invariant #3, boto3 is
/// bundled rather than taken from the runtime, so the artifact size and SDK version
/// are honest and pinned.
fn build_python(repo_root: &Path, dist: &Path, key: &ArtifactKey) -> Result<Artifact> {
    let scenario = key.scenario.as_str();
    let src_dir = repo_root.join("scenarios/python").join(scenario);
    let handler = src_dir.join("handler.py");
    if !handler.is_file() {
        bail!("missing python handler at {}", handler.display());
    }

    // Stage into a per-label build dir (gitignored) so the two arch builds of an
    // arch-specific scenario (authz) get distinct trees rather than overwriting
    // one another's pip-installed native wheel. Cleaned each build.
    let staging = repo_root.join("scenarios/python/.build").join(key.label());
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("creating python staging dir {}", staging.display()))?;

    std::fs::copy(&handler, staging.join("handler.py"))
        .with_context(|| format!("copying {} into staging", handler.display()))?;

    // authz embeds the RSA public verification key (the same generated JWK the
    // other languages embed). The bencher's build.rs runs the fixture generator
    // before this binary builds, so the file exists here; fail loud if not.
    if key.scenario == Scenario::Authz {
        let jwk = repo_root.join("bencher/fixtures/authz_public_jwk.json");
        if !jwk.is_file() {
            bail!(
                "authz public JWK not found at {} (run `node bencher/fixtures/generate.mjs`)",
                jwk.display()
            );
        }
        std::fs::copy(&jwk, staging.join("authz_public_jwk.json"))
            .with_context(|| format!("copying authz JWK from {}", jwk.display()))?;
    }

    // Install pinned dependencies for the Lambda target into the staging dir, if
    // the scenario declares any. `hello` has none.
    let requirements = src_dir.join("requirements.txt");
    if requirements.is_file() {
        pip_install_target(&requirements, &staging, key.arch)?;
    }

    let unzipped_size_bytes = walk(&staging)?
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let zip_path = dist.join(format!("{}.zip", key.label()));
    zip_dir(&staging, &zip_path)?;
    let zip_size_bytes = std::fs::metadata(&zip_path)?.len();
    Ok(Artifact {
        key: *key,
        zip_path,
        zip_size_bytes,
        unzipped_size_bytes,
    })
}

/// Cross-compiles one Go scenario for one architecture into a `bootstrap`
/// executable and zips it for the `provided.al2023` custom runtime.
///
/// Cross-compiles for the Lambda target (`GOOS=linux`, the cell's `GOARCH`) so it
/// works on macOS. `CGO_ENABLED=0` yields a fully static binary independent of the
/// host libc; the `lambda.norpc` tag drops the legacy net/rpc component (only the
/// deprecated go1.x runtime needs it); and
/// `-ldflags=-s -w -trimpath` strips symbols, DWARF, and build paths, so the
/// artifact ships only loadable code (matching Rust's `strip` and Node's
/// minification, so the artifact-size comparison stays fair).
fn build_go(repo_root: &Path, dist: &Path, key: &ArtifactKey) -> Result<Artifact> {
    let scenario = key.scenario.as_str();
    // All Go scenarios share one module (`scenarios/go`), each a `package main`
    // subdir built as `./<scenario>`. Go's linker drops unused packages per binary,
    // so the shared module does not bloat `hello` with the SDK.
    let go_root = repo_root.join("scenarios/go");
    let src_dir = go_root.join(scenario);
    if !src_dir.join("main.go").is_file() {
        bail!(
            "missing go handler at {}",
            src_dir.join("main.go").display()
        );
    }

    // authz embeds the RSA public verification key. Go's `//go:embed` cannot
    // reference a path outside the module, so stage the JWK into the authz package
    // dir before building (gitignored). The bencher's build.rs generates it first,
    // so it exists here; fail loud if not.
    if key.scenario == Scenario::Authz {
        let jwk = repo_root.join("bencher/fixtures/authz_public_jwk.json");
        if !jwk.is_file() {
            bail!(
                "authz public JWK not found at {} (run `node bencher/fixtures/generate.mjs`)",
                jwk.display()
            );
        }
        std::fs::copy(&jwk, src_dir.join("authz_public_jwk.json"))
            .with_context(|| format!("copying authz JWK from {}", jwk.display()))?;
    }

    let goarch = match key.arch {
        Arch::Arm64 => "arm64",
        Arch::X86_64 => "amd64",
    };

    // Deterministic, unique output path per (scenario, arch) so concurrent/
    // sequential arch builds never overwrite each other's `bootstrap`. Lives
    // under the gitignored dist/ tree.
    let out_dir = dist.join(format!("go-build-{}", key.label()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("creating go build dir {}", out_dir.display()))?;
    let bootstrap = out_dir.join("bootstrap");

    let mut cmd = Command::new("go");
    cmd.current_dir(&go_root)
        .args([
            "build",
            "-tags",
            "lambda.norpc",
            "-trimpath",
            "-ldflags=-s -w",
            "-o",
        ])
        .arg(&bootstrap)
        .arg(format!("./{scenario}"))
        .env("GOOS", "linux")
        .env("GOARCH", goarch)
        .env("CGO_ENABLED", "0");
    run(cmd, &format!("go build {}", key.label()))?;

    if !bootstrap.is_file() {
        bail!("expected bootstrap at {}", bootstrap.display());
    }
    let unzipped_size_bytes = std::fs::metadata(&bootstrap)?.len();
    let zip_path = dist.join(format!("{}.zip", key.label()));
    zip_single(&bootstrap, "bootstrap", &zip_path, 0o755)?;
    let zip_size_bytes = std::fs::metadata(&zip_path)?.len();
    Ok(Artifact {
        key: *key,
        zip_path,
        zip_size_bytes,
        unzipped_size_bytes,
    })
}

/// Runs `pip install --target` to lay a scenario's dependencies into its staging
/// dir, resolving wheels for the Lambda runtime (`python3.14`, CPython, the given
/// architecture's manylinux platform) rather than the host. `--only-binary=:all:`
/// forces wheel-only resolution so no source build runs against the host
/// toolchain (which would produce host-arch, not Lambda-arch, native code). Both
/// the manylinux2014 (glibc 2.17) and manylinux_2_28 platform tags are offered so
/// pip can pick whichever a native wheel (e.g. `cryptography`) publishes.
fn pip_install_target(requirements: &Path, target: &Path, arch: Arch) -> Result<()> {
    let manylinux_arch = match arch {
        Arch::Arm64 => "aarch64",
        Arch::X86_64 => "x86_64",
    };
    let mut cmd = Command::new("python3");
    cmd.args(["-m", "pip", "install", "--target"])
        .arg(target)
        .args(["--python-version", "3.14"])
        .args(["--implementation", "cp"])
        .arg("--only-binary=:all:")
        .args(["--platform", &format!("manylinux2014_{manylinux_arch}")])
        .args(["--platform", &format!("manylinux_2_28_{manylinux_arch}")])
        .arg("-r")
        .arg(requirements);
    run(
        cmd,
        &format!(
            "pip install python deps ({manylinux_arch}) from {}",
            requirements.display()
        ),
    )
}

/// Returns the single `.zip` file in a directory, erroring if there is not
/// exactly one. Gradle's `buildZip` produces one distribution zip per
/// subproject; anything else means a stale/ambiguous build tree.
fn single_zip_in(dir: &Path) -> Result<PathBuf> {
    let mut zips: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "zip").unwrap_or(false))
        .collect();
    match zips.len() {
        1 => Ok(zips.pop().unwrap()),
        0 => bail!("no .zip found in {}", dir.display()),
        n => bail!("expected exactly one .zip in {}, found {n}", dir.display()),
    }
}

/// Invokes the project's gradle wrapper for a single codegen task (the Rust/Node
/// Smithy SSDK generation). Thin wrapper over `run_gradle` for the one-task case.
fn gradle_codegen(project_dir: &Path, task: &str) -> Result<()> {
    run_gradle(
        project_dir,
        std::slice::from_ref(&task.to_string()),
        &format!("gradle {task} in {}", project_dir.display()),
    )
}

// ----- small process / fs helpers -----

/// Runs the project's gradle wrapper (`./gradlew`) in `project_dir` with the
/// given tasks, in plain-console mode. Every gradle project here (the Java
/// artifact build and the Rust/Node Smithy codegen) runs a Gradle 9.x wrapper on
/// whatever JDK is on PATH (JDK 25 both on a dev Mac and in the runner
/// container), so no JAVA_HOME juggling is needed - this is the single entry
/// point for invoking gradle.
fn run_gradle(project_dir: &Path, tasks: &[String], what: &str) -> Result<()> {
    let mut cmd = Command::new("./gradlew");
    cmd.current_dir(project_dir).arg("--console=plain");
    cmd.args(tasks);
    run(cmd, what)
}

fn cmd_in(dir: &Path, program: &str, args: &[&str]) -> Command {
    let mut c = Command::new(program);
    c.current_dir(dir).args(args);
    c
}

fn run(mut cmd: Command, what: &str) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawning: {what}"))?;
    if !status.success() {
        bail!("{what} failed with status {status}");
    }
    Ok(())
}

fn capture(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().context("capturing command output")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Captures a tool's `--version` output for the reproducibility manifest,
/// mapping any failure (tool absent, non-UTF-8, empty output) to the explicit
/// sentinel `"<unavailable>"` so a missing toolchain is never recorded as `""`.
fn tool_version(cmd: &mut Command) -> String {
    match capture(cmd) {
        Ok(v) if !v.is_empty() => v,
        _ => "<unavailable>".to_string(),
    }
}

fn walk(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in
            std::fs::read_dir(&d).with_context(|| format!("reading dir {}", d.display()))?
        {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// Sums the uncompressed size of every entry in a zip, read from the central
/// directory (no inflation). Used to recover the unzipped artifact size for a
/// `--skip-build` run, which reuses zips already on disk.
fn unzipped_size_from_zip(zip_path: &Path) -> Result<u64> {
    let file =
        std::fs::File::open(zip_path).with_context(|| format!("opening {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", zip_path.display()))?;
    let mut total = 0u64;
    for i in 0..archive.len() {
        total += archive.by_index(i)?.size();
    }
    Ok(total)
}

/// Zips a single file under a chosen archive name with the given unix mode.
fn zip_single(src: &Path, arcname: &str, zip_path: &Path, mode: u32) -> Result<()> {
    let file = std::fs::File::create(zip_path)
        .with_context(|| format!("creating {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(mode);
    zip.start_file(arcname, opts)?;
    let mut buf = Vec::new();
    std::fs::File::open(src)?.read_to_end(&mut buf)?;
    zip.write_all(&buf)?;
    zip.finish()?;
    Ok(())
}

/// Zips every file in a directory (flat, preserving relative paths).
fn zip_dir(dir: &Path, zip_path: &Path) -> Result<()> {
    let file = std::fs::File::create(zip_path)
        .with_context(|| format!("creating {}", zip_path.display()))?;
    let mut zip = zip::ZipWriter::new(file);
    let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for path in walk(dir)? {
        let rel = path.strip_prefix(dir)?.to_string_lossy().into_owned();
        zip.start_file(rel, opts)?;
        let mut buf = Vec::new();
        std::fs::File::open(&path)?.read_to_end(&mut buf)?;
        zip.write_all(&buf)?;
    }
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unzipped_size_from_zip` reads the uncompressed total from the central
    /// directory: it must equal the sum of the original file sizes, not the
    /// (smaller) compressed size on disk.
    #[test]
    fn unzipped_size_sums_uncompressed_entries() {
        let dir = std::env::temp_dir().join(format!("lambdabench-zip-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("payload.bin");
        // Highly compressible content so the zip is much smaller than the source,
        // proving the result is the uncompressed size, not the on-disk zip size.
        let original = vec![0u8; 100_000];
        std::fs::write(&src, &original).unwrap();
        let zip_path = dir.join("payload.zip");
        zip_single(&src, "payload.bin", &zip_path, 0o644).unwrap();

        let unzipped = unzipped_size_from_zip(&zip_path).unwrap();
        let compressed = std::fs::metadata(&zip_path).unwrap().len();
        assert_eq!(unzipped, original.len() as u64);
        assert!(
            compressed < unzipped,
            "expected the compressed zip ({compressed} B) to be smaller than the unzipped total ({unzipped} B)"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
