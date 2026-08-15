# Build with the UQA-RS repository root as Docker build context:
#   docker build -f integrations/cairn/Dockerfile .
# Warm UQA support is opt-in and follows the UQA-RS license in that checkout:
#   docker build -f integrations/cairn/Dockerfile --build-arg CAIRN_FEATURES=uqa .
FROM rust:1.90-bookworm AS build
WORKDIR /src
COPY . .
ARG CAIRN_FEATURES=""
RUN if [ -n "$CAIRN_FEATURES" ]; then \
      cargo build --release --manifest-path integrations/cairn/Cargo.toml --bin cairn --features "$CAIRN_FEATURES"; \
    else \
      cargo build --release --manifest-path integrations/cairn/Cargo.toml --bin cairn --no-default-features; \
    fi

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/integrations/cairn/target/release/cairn /usr/local/bin/cairn
VOLUME ["/data/cairn"]
EXPOSE 8080
ENTRYPOINT ["cairn"]
CMD ["serve", "--listen", "0.0.0.0:8080", "--local-store", "/data/cairn/store", "--cache", "/data/cairn/cache"]
