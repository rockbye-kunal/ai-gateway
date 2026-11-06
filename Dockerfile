# syntax=docker/dockerfile:1
FROM rust:1.85-bookworm AS build
WORKDIR /src
# Cache dependencies separately from source.
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo "fn main() {}" > src/main.rs && echo "" > src/lib.rs \
    && cargo build --release || true
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release

# Distroless: no shell, no package manager, runs as an unprivileged user.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/ai-gateway /usr/local/bin/ai-gateway
USER nonroot:nonroot
EXPOSE 8080 9090
ENV GATEWAY_CONFIG=/etc/ai-gateway/config.yaml
ENTRYPOINT ["/usr/local/bin/ai-gateway"]
CMD ["serve"]
