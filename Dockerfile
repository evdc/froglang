FROM rust:1.82 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p froglang-core -p playground-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libgcc-s1 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/froglang-core /usr/local/bin/
COPY --from=builder /app/target/release/playground-server /usr/local/bin/
COPY playground-ui/ /app/ui/
WORKDIR /app
EXPOSE 8080
CMD ["playground-server"]
