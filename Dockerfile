# ---- build ----
FROM rust:1.98.1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src ./src
RUN cargo build --release --locked --bin prusa-watch

# ---- runtime ----
FROM debian:bookworm-slim
# ffmpeg: decodes the Buddy3D RTSP stream; ca-certificates: TLS for ntfy/Discord/model download; curl: healthcheck
RUN apt-get update \
 && apt-get install -y --no-install-recommends ffmpeg ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/prusa-watch /usr/local/bin/prusa-watch

WORKDIR /app
# Bake the model into the image so the container needs no internet at runtime
# (fetch-model also checks that it loads).
RUN prusa-watch fetch-model

ENV PRUSA_WATCH_CONFIG=/config/config.yaml
VOLUME ["/app/data"]
EXPOSE 8484

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
  CMD curl -fsS -o /dev/null http://127.0.0.1:8484/healthz || exit 1

ENTRYPOINT ["prusa-watch"]
CMD ["run"]
