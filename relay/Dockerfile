# The relay as a static binary in an empty image. Build from the repo root:
#   docker build -f relay/Dockerfile .
# The build stage runs natively and cross-compiles with zig, so building an
# amd64 image on an arm64 Mac (or the reverse) needs no emulation.
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:0.23 AS build
ARG TARGETARCH
WORKDIR /src
COPY . .
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-musl ;; \
      arm64) target=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported arch $TARGETARCH" >&2; exit 1 ;; \
    esac \
  && rustup target add "$target" \
  && cargo zigbuild --release --locked -p zeron-relay --target "$target" \
  && cp "target/$target/release/zeron-relay" /zeron-relay

FROM scratch
COPY --from=build /zeron-relay /zeron-relay
ENV BIND_ADDR=0.0.0.0:8080 DATA_DIR=/data
EXPOSE 8080
USER 10001:10001
ENTRYPOINT ["/zeron-relay"]
