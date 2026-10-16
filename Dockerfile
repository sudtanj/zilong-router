# Local multi-arch build: docker buildx build --platform linux/arm64 -t mini-router .
# The published image is built by sudtanj/infrastructure-tools (builder-tools/mini-router),
# which uses the same recipe but clones the source instead of copying it.
FROM --platform=$BUILDPLATFORM rust:1-bookworm AS build
ARG TARGETARCH
ARG BUILDARCH
WORKDIR /src
COPY . .
# Cross-compile on the build platform (glibc target; ring and bundled SQLite need a C cross
# compiler plus target headers).
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) T=x86_64-unknown-linux-gnu;  CC=x86_64-linux-gnu-gcc;  P="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" ;; \
      arm64) T=aarch64-unknown-linux-gnu; CC=aarch64-linux-gnu-gcc; P="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" ;; \
      *) echo "unsupported TARGETARCH=$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      apt-get update && apt-get install -y --no-install-recommends $P && rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$T"; \
    U="$(echo "$T" | tr 'a-z-' 'A-Z_')"; \
    export "CARGO_TARGET_${U}_LINKER=$CC" "CC_$(echo "$T" | tr '-' '_')=$CC"; \
    cargo build --release --locked --target "$T"; \
    install -m755 "target/$T/release/mini-router" /mini-router; \
    install -d -o 65532 -g 65532 /data

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /mini-router /usr/local/bin/mini-router
COPY --from=build --chown=65532:65532 /data /data
ENV DATA_DIR=/data HOSTNAME=0.0.0.0 PORT=20128 MINI_ROUTER_WORKERS=1
USER 65532:65532
VOLUME /data
EXPOSE 20128
ENTRYPOINT ["/usr/local/bin/mini-router"]
