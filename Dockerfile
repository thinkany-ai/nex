# nex relay. Build: docker build -t nex-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p nex-server

FROM debian:bookworm-slim
RUN useradd --system --home /var/lib/nex nex && mkdir -p /var/lib/nex && chown nex /var/lib/nex
COPY --from=build /src/target/release/nex-server /usr/local/bin/nex-server
USER nex
ENV NEX_LISTEN=0.0.0.0:8080 NEX_DB=/var/lib/nex/nex.db
VOLUME /var/lib/nex
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 && printf 'GET /healthz HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ok <&3"]
CMD ["nex-server"]
