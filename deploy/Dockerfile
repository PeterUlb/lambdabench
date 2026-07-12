# Benchmark-runner image for LambdaBench.
#
# Carries the full toolchain that `cargo run -p bencher -- doctor` verifies, so a
# single ECS Fargate task can build every language's Lambda artifact, deploy the
# function matrix, run the hours-long benchmark, tear everything down, and then
# rebuild and publish the static site. Built for linux/amd64 (the Fargate task platform).
#
# Toolchain pinned to what the repo requires:
#   Rust + cargo-lambda (+ zig, the cross-compile linker for arm64 & x86_64)
#   Node 24 + npm        (esbuild bundling; also builds the site)
#   JDK 25               (scenarios/java targets JavaLanguageVersion 25)
#   Python 3.14 + pip    (pip install --target per scenario/arch)
#   Go 1.26              (scenarios/go go.mod)
#   awscli v2            (site sync + CloudFront invalidation in the publish step)
#   crane                (daemonless OCI push for the zip-vs-image probe family)
#
# Tracks the 24.04 tag (not a digest) so the base keeps receiving security updates;
# the layers above pull current toolchains at build time regardless, so a digest pin
# would freeze only one link of a chain that already floats. Runs as root by design:
# this is an ephemeral, single-tenant Fargate task whose toolchain (rustup/cargo, pip)
# installs under /root, and it holds no long-lived secrets. There is no multi-tenant
# surface for privilege separation to protect.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    PATH=/root/.cargo/bin:/usr/local/go/bin:$PATH \
    CARGO_TERM_COLOR=never

# --- Base packages -----------------------------------------------------------
# rsync is used by the bencher to lift the Node Smithy SSDK out of gradle's
# tracked output dir before npm/esbuild bundles it (build.rs::copy_ssdk).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git unzip xz-utils zip rsync \
      build-essential pkg-config libssl-dev \
      software-properties-common gnupg \
    && rm -rf /var/lib/apt/lists/*

# --- Python 3.14 -------------------------------------------------------------
# deadsnakes carries point releases ahead of the Ubuntu archive.
RUN add-apt-repository -y ppa:deadsnakes/ppa \
    && apt-get update && apt-get install -y --no-install-recommends \
      python3.14 python3.14-venv python3.14-dev \
    && rm -rf /var/lib/apt/lists/* \
    && update-alternatives --install /usr/bin/python3 python3 /usr/bin/python3.14 1 \
    && curl -sS https://bootstrap.pypa.io/get-pip.py | python3.14

# --- Node 24 -----------------------------------------------------------------
# corepack enables yarn (and pnpm) via the package-manager shim Node ships with;
# the bencher uses `yarn install` / `yarn run` to build the Smithy TS SSDK.
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && corepack enable

# --- JDK 25 (Temurin) --------------------------------------------------------
RUN curl -fsSL https://packages.adoptium.net/artifactory/api/gpg/key/public \
      | gpg --dearmor -o /etc/apt/keyrings/adoptium.gpg \
    && echo "deb [signed-by=/etc/apt/keyrings/adoptium.gpg] https://packages.adoptium.net/artifactory/deb noble main" \
      > /etc/apt/sources.list.d/adoptium.list \
    && apt-get update && apt-get install -y --no-install-recommends temurin-25-jdk \
    && rm -rf /var/lib/apt/lists/*
ENV JAVA_HOME=/usr/lib/jvm/temurin-25-jdk-amd64

# --- Go 1.26 -----------------------------------------------------------------
# Version + sha256 pinned so a rebuild produces the same toolchain; bump both
# together (the checksum is listed next to the file on https://go.dev/dl/).
ARG GO_VERSION=1.26.4
ARG GO_SHA256=1153d3d50e0ac764b447adfe05c2bcf08e889d42a02e0fe0259bd47f6733ad7f
RUN curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-amd64.tar.gz" -o /tmp/go.tgz \
    && echo "${GO_SHA256}  /tmp/go.tgz" | sha256sum -c - \
    && tar -C /usr/local -xzf /tmp/go.tgz \
    && rm /tmp/go.tgz

# --- Rust + cargo-lambda + zig ----------------------------------------------
# zig is cargo-lambda's cross-compile linker for the provided.al2023 targets
# (both aarch64 and x86_64). cargo-lambda is installed from crates.io. These two
# helper versions (zig, cargo-lambda) are pinned; bump explicitly. rustc itself
# tracks `stable` at build time, part of the toolchain chain the header notes
# already floats (the per-run build manifest records the resolved versions).
ARG ZIGLANG_VERSION=0.16.0
ARG CARGO_LAMBDA_VERSION=1.9.1
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal \
    && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
    && pip3 install --no-cache-dir "ziglang==${ZIGLANG_VERSION}" \
    && cargo install --locked --version "${CARGO_LAMBDA_VERSION}" cargo-lambda

# --- AWS CLI v2 --------------------------------------------------------------
RUN curl -fsSL "https://awscli.amazonaws.com/awscli-exe-linux-x86_64.zip" -o /tmp/awscliv2.zip \
    && unzip -q /tmp/awscliv2.zip -d /tmp \
    && /tmp/aws/install \
    && rm -rf /tmp/aws /tmp/awscliv2.zip

# --- crane (google/go-containerregistry) ------------------------------------
# `bencher probe download-scaling --with-image` (the zip-vs-container-image
# family) assembles each padded image with `crane mutate` (pull base + append one
# layer + set CMD + push to ECR) instead of a container build. crane is a single
# static Go binary and daemonless, so unlike finch/Docker it needs no VM or
# privileged container and runs unattended in this Fargate task. Version + sha256
# pinned so a rebuild produces the same binary; bump both together (checksums.txt
# for the release lists the amd64 digest).
ARG CRANE_VERSION=0.21.7
ARG CRANE_SHA256=1a57bc98207fa1c0d04bf760699099e26f8383499bfd55b99c1b919a928a7230
RUN curl -fsSL "https://github.com/google/go-containerregistry/releases/download/v${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz" -o /tmp/crane.tgz \
    && echo "${CRANE_SHA256}  /tmp/crane.tgz" | sha256sum -c - \
    && tar -xzf /tmp/crane.tgz -C /usr/local/bin crane \
    && rm /tmp/crane.tgz

# --- Project -----------------------------------------------------------------
WORKDIR /lambdabench
COPY . /lambdabench

# The entrypoint runs the whole pipeline and publishes the site.
RUN chmod +x /lambdabench/deploy/run-benchmark.sh
CMD ["/lambdabench/deploy/run-benchmark.sh"]
