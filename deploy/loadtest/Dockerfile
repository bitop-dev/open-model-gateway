# Load-test image (docs/operations.md "Capacity baseline"): the gateway plus
# the test-only mock upstream and load generator, API only (no SPA). Never a
# production image. Build from the repository root:
#   docker build -f deploy/loadtest/Dockerfile -t omg-loadtest:local .
# No Docker Hub pulls (docs/verification.md): Google's mirror and distroless.
FROM mirror.gcr.io/library/rust:1.100.0-bookworm@sha256:75d26c23c24cc6ea1cdabf0714a6bff8fd7bf3248716043fd7ae86dac7f41b94 AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY apps/gateway/ apps/gateway/
COPY crates/ crates/
COPY vendor/ vendor/
# Embedded by `open-model-gateway grants apply|verify`.
COPY deploy/staging/runtime-grants.sql deploy/staging/verify-privileges.sql deploy/staging/
COPY tools/ tools/
ARG CARGO_BUILD_JOBS=8
# Same release profile as the production image (thin LTO, one codegen unit).
RUN --mount=type=cache,id=omg-loadtest-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=omg-loadtest-target,target=/build/target \
    cargo build --locked --release --jobs "$CARGO_BUILD_JOBS" \
      -p open-model-gateway -p omg-mock-upstream -p omg-loadgen \
 && mkdir -p /out \
 && cp target/release/open-model-gateway target/release/mock-upstream target/release/loadgen /out/

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:29ee70762214fe8190cea04ef1c0a84a8365879e047ce8f911ae5a5ab2355fa2
COPY --from=build --chmod=0555 /out/ /usr/local/bin/
ENV GATEWAY_ENV=production \
    GATEWAY_LISTEN=0.0.0.0:8080 \
    HOME=/nonexistent
USER 10001:10001
EXPOSE 8080 9464 8000
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/open-model-gateway"]
CMD ["serve"]
