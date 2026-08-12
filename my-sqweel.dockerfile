# harness-hat Rust image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-mysqweel:local -f my-sqweel.dockerfile .

# Rust's official manifest is pinned and selects the matching target
# architecture. The copied rustup/cargo trees retain side-by-side toolchain
# management without manually downloading rustup-init per architecture.
FROM harness-hat-base:local

USER root

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

ARG MARIADB_PACKAGE_VERSION=1:10.11.7-2ubuntu2

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      cmake \
      pkg-config \
      clang \
      lld \
      mold \
      gdb \
      lldb \
      protobuf-compiler \
      sqlite3 \
      libsqlite3-dev \
      libssl-dev \
      libnuma1 \
      libpcre2-8-0 \
      libpcre2-posix3 \
      libpmem1 \
      liburing2 \
      perl \
      zlib1g \
      mariadb-client="${MARIADB_PACKAGE_VERSION}" \
      mariadb-server="${MARIADB_PACKAGE_VERSION}" \
      mariadb-test="${MARIADB_PACKAGE_VERSION}" \
      mariadb-test-data="${MARIADB_PACKAGE_VERSION}" \
      jq \
      shellcheck \
      direnv; \
    test -x /usr/bin/mysqltest; \
    test -x /usr/share/mysql/mysql-test/mariadb-test-run.pl; \
    test -x /usr/share/mysql/mysql-test/lib/My/SafeProcess/my_safe_process; \
    test -x /usr/sbin/mariadbd; \
    /usr/bin/mysqltest --version; \
    rm -rf /var/lib/apt/lists/*

# The helper starts a disposable local MariaDB instance for parity and MTR
# runs when no external comparison server is supplied. The workspace remains
# mounted by Harness Hat, but installing this copy makes the image usable on
# its own as well.
COPY vendor/mysql-test-server.sh /usr/local/bin/mysql-test-server
RUN chmod +x /usr/local/bin/mysql-test-server

# Rust installs as root into the shared RUSTUP_HOME/CARGO_HOME. The container
# runs as `coder` (uid 1000), so hand ownership of both trees to that user in
# this same layer — otherwise `cargo build` (registry/cache writes), `cargo
# install` (writes to cargo/bin), and `rustup` updates all fail on root-owned
# paths. Doing the chown here (not a later layer) avoids duplicating the large
# toolchain with new ownership. a+rX keeps it readable if run under another uid.
# Pinned versions (H5): the official multi-architecture Rust image is pinned
# by manifest digest; rustup verifies component signatures/hashes internally.
# The cargo tools remain exact-version pins. Bump the image tag and digest
# together when updating the toolchain.
ARG RUST_TOOLCHAIN=1.97.0
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=rust /usr/local/rustup /usr/local/rustup
RUN set -eu; \
    rustup toolchain install "${RUST_TOOLCHAIN}" --profile default; \
    rustup default "${RUST_TOOLCHAIN}"; \
    rustup component add rustfmt clippy rust-src rust-analyzer; \
    cargo install --locked \
      cargo-edit@0.13.11 \
      cargo-watch@8.5.3 \
      cargo-nextest@0.9.140 \
      cargo-audit@0.22.2 \
      cargo-deny@0.20.2; \
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    chown -R coder:coder "${RUSTUP_HOME}" "${CARGO_HOME}"

USER coder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
