# ansible-operator task runner. Run `just` to list recipes.

# Deny broken intra-doc links / bare URLs in the API reference (rustdoc).
export RUSTDOCFLAGS := "-D rustdoc::broken_intra_doc_links -D rustdoc::bare_urls"

# List available recipes.
default:
    @just --list

# Build the user & operator guide (the mdBook under docs/) to docs/book/.
docs:
    mdbook build docs

# Serve the guide locally with live reload (http://localhost:3000) and open it.
docs-serve:
    mdbook serve docs --open

# Build the generated API reference (rustdoc) for the operator's internals.
apidoc:
    cargo doc --no-deps --document-private-items

# Compile the operator.
build:
    cargo build

# Run the unit tests.
test:
    cargo test

# Lint (must stay clean — see AGENTS.md).
clippy:
    cargo clippy

# The full pre-change gate: build + test + clippy + guide + API docs.
check: build test clippy docs apidoc

# Dump all CRDs (PlaybookPlan, Play, ClusterInventory, StaticInventory, NodeAccessPolicy) to stdout.
crds:
    cargo run --quiet -- crds
