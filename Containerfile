FROM docker.io/rust:1-bookworm as builder
COPY . /opt
WORKDIR /opt
RUN cargo build --profile release

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /opt/target/release/ansible-operator /
ENTRYPOINT ["/ansible-operator"]