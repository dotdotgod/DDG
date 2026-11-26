# syntax=docker/dockerfile:1.7

# =============================================================================
# Stage 1: Build libghostty-vt 1.0.1 statically from official release tarballs
# =============================================================================
FROM --platform=$BUILDPLATFORM debian:bookworm-slim AS vt-builder
ARG BUILDARCH

ENV ZIG_VERSION=0.17.1 \
    GHOSTTY_VT_VERSION=1.0.1

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
RUN set -eux; \
    case "${BUILDARCH}" in \
        arm64) ZIG_ARCH="aarch64"; ZIG_SHA="a1546d5646eb184437918d6635ca6b762a0e889eac09cef720ee7a1ed3fe7795" ;; \
        amd64) ZIG_ARCH="x86_64";  ZIG_SHA="3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a" ;; \
        *) echo "Unsupported BUILDARCH: ${BUILDARCH}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-${ZIG_ARCH}-linux-${ZIG_VERSION}.tar.xz" -o zig.tar.xz; \
    echo "${ZIG_SHA}  zig.tar.xz" | sha256sum -c -; \
    mkdir -p /opt/zig; \
    tar -xJf zig.tar.xz --strip-components=1 -C /opt/zig; \
    rm zig.tar.xz; \
    curl -fsSL "https://release.files.ghostty.org/libghostty-vt/${GHOSTTY_VT_VERSION}/libghostty-vt-${GHOSTTY_VT_VERSION}.tar.gz" -o vt.tar.gz; \
    curl -fsSL "https://release.files.ghostty.org/libghostty-vt/${GHOSTTY_VT_VERSION}/libghostty-vt-${GHOSTTY_VT_VERSION}-deps.tar.gz" -o vt-deps.tar.gz; \
    echo "333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa  vt.tar.gz" | sha256sum -c -; \
    echo "91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d  vt-deps.tar.gz" | sha256sum -c -; \
    mkdir -p /src/vt /src/deps; \
    tar -xzf vt.tar.gz --strip-components=1 -C /src/vt; \
    tar -xzf vt-deps.tar.gz -C /src/deps; \
    rm vt.tar.gz vt-deps.tar.gz

ARG TARGETARCH
RUN set -eux; \
    case "${TARGETARCH}" in \
        arm64) ZIG_TARGET="aarch64-linux-gnu.2.36" ;; \
        amd64) ZIG_TARGET="x86_64-linux-gnu.2.36" ;; \
        *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    DEPS_DIR="/src/deps"; \
    if [ -d "/src/deps/p" ]; then DEPS_DIR="/src/deps/p"; \
    elif [ -d "/src/deps/libghostty-vt-${GHOSTTY_VT_VERSION}-deps/p" ]; then DEPS_DIR="/src/deps/libghostty-vt-${GHOSTTY_VT_VERSION}-deps/p"; \
    elif [ -d "/src/deps/libghostty-vt-${GHOSTTY_VT_VERSION}-deps" ]; then DEPS_DIR="/src/deps/libghostty-vt-${GHOSTTY_VT_VERSION}-deps"; fi; \
    cd /src/vt; \
    /opt/zig/zig build \
        -Dtarget="${ZIG_TARGET}" \
        -Demit-lib-vt \
        -Demit-exe=false \
        -Doptimize=ReleaseFast \
        -Dcpu=baseline \
        --system "${DEPS_DIR}" \
        --prefix /out \
        --cache-dir /tmp/zig-cache; \
    ls -la /out/lib

# =============================================================================
# Stage 2: Compile ddg-terminal-server and ddg-agents-host
# =============================================================================
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS rust-builder
ARG BUILDARCH
ARG TARGETARCH

COPY --from=vt-builder /out/lib/libghostty-vt.a /opt/ghostty-vt/lib/libghostty-vt.a

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Exclude macOS/desktop GPUI client from the headless Linux server image.
RUN sed -i '/"native",/d' Cargo.toml

ENV GHOSTTY_VT_LIB_DIR=/opt/ghostty-vt/lib
RUN set -eux; \
    set --; \
    RELEASE_DIR=target/release; \
    STRIP=strip; \
    if [ "${TARGETARCH}" != "${BUILDARCH}" ]; then \
        case "${TARGETARCH}" in \
            amd64) RUST_TARGET=x86_64-unknown-linux-gnu; CROSS_ARCH=x86_64; GCC_PACKAGE=gcc-x86-64-linux-gnu ;; \
            arm64) RUST_TARGET=aarch64-unknown-linux-gnu; CROSS_ARCH=aarch64; GCC_PACKAGE=gcc-aarch64-linux-gnu ;; \
            *) echo "Unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
        esac; \
        CROSS_PREFIX="${CROSS_ARCH}-linux-gnu"; \
        apt-get update; \
        apt-get install -y --no-install-recommends "${GCC_PACKAGE}" "libc6-dev-${TARGETARCH}-cross"; \
        rm -rf /var/lib/apt/lists/*; \
        rustup target add "${RUST_TARGET}"; \
        TARGET_ENV="$(printf '%s' "${RUST_TARGET}" | tr '-' '_')"; \
        UPPER_TARGET="$(printf '%s' "${TARGET_ENV}" | tr '[:lower:]' '[:upper:]')"; \
        export "CARGO_TARGET_${UPPER_TARGET}_LINKER=${CROSS_PREFIX}-gcc"; \
        export "CC_${TARGET_ENV}=${CROSS_PREFIX}-gcc" "AR_${TARGET_ENV}=${CROSS_PREFIX}-ar"; \
        set -- --target "${RUST_TARGET}"; \
        RELEASE_DIR="target/${RUST_TARGET}/release"; \
        STRIP="${CROSS_PREFIX}-strip"; \
    fi; \
    cargo build --release "$@" --bin ddg-terminal-server --bin ddg-agents-host; \
    install -Dt /out/bin "${RELEASE_DIR}/ddg-terminal-server" "${RELEASE_DIR}/ddg-agents-host"; \
    "${STRIP}" /out/bin/ddg-terminal-server /out/bin/ddg-agents-host

FROM scratch AS binaries
COPY --from=rust-builder /out/bin/ddg-terminal-server /ddg-terminal-server
COPY --from=rust-builder /out/bin/ddg-agents-host /ddg-agents-host

# Release jobs use native amd64/arm64 runners; reuse the Bookworm build cache.
FROM rust-builder AS release-builder
ARG BUILDARCH
ARG TARGETARCH
RUN test "${TARGETARCH}" = "${BUILDARCH}" \
    && cargo build --locked --release --bin ddg \
    && install -Dt /out/bin target/release/ddg \
    && strip /out/bin/ddg

FROM scratch AS release-binaries
COPY --from=release-builder /out/bin/ /
COPY THIRD_PARTY_NOTICES.txt /THIRD_PARTY_NOTICES.txt

# =============================================================================
# Stage 3: Minimal non-root runtime image (tini + optional hardened sshd)
# =============================================================================
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        tini \
        openssh-server \
        zsh \
        bash \
        git \
        locales \
    && sed -i '/en_US.UTF-8/s/^# //g' /etc/locale.gen \
    && locale-gen \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -m -u 1000 -s /bin/zsh ddg \
    && mkdir -p /home/ddg/.local/state/ddg /home/ddg/.ssh /etc/ddg/ssh /etc/ddg/plugins /workspace \
    && chown -R ddg:ddg /home/ddg /etc/ddg /workspace \
    && chmod 700 /home/ddg/.local/state/ddg /home/ddg/.ssh /etc/ddg/ssh

COPY --from=rust-builder /out/bin/ddg-terminal-server /usr/local/bin/ddg-terminal-server
COPY --from=rust-builder /out/bin/ddg-agents-host /usr/local/bin/ddg-agents-host
COPY docker/sshd_config /etc/ddg/sshd_config
COPY --chmod=755 docker/entrypoint.sh /usr/local/bin/ddg-entrypoint.sh

ENV LANG=en_US.UTF-8 \
    LC_ALL=en_US.UTF-8 \
    TERM=xterm-256color \
    DDG_STATE_DIR=/home/ddg/.local/state/ddg \
    DDG_RUN_DIR=/home/ddg/.local/state/ddg \
    DDG_PLUGINS_DIR=/etc/ddg/plugins \
    DDG_ENABLE_SSHD=0

USER ddg
WORKDIR /workspace
EXPOSE 2222

HEALTHCHECK --interval=15s --timeout=3s --start-period=5s --retries=3 \
    CMD ["ddg-agents-host", "health"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ddg-entrypoint.sh"]
