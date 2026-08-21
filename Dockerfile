FROM rust:alpine

WORKDIR /src
COPY . .

RUN cargo test --all-targets
