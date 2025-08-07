FROM gcr.io/distroless/cc-debian12
COPY target/release/ansible-operator /
ENTRYPOINT ["/ansible-operator"]