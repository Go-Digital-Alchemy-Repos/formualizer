# Engine development worker for Railway: Rust toolchain, Python, maturin.
# Persistent state (cargo target dir, wheelhouse, private workbooks) lives on
# the volume mounted at /data; the source is the repo checkout at /src.
FROM docker.io/library/rust@sha256:7274e0edb5b47eda8053b350ebf3d489f7e0f65d2d7e77b16076299c7c047c28
RUN apt-get update && apt-get install -y --no-install-recommends \
      python3 python3-venv python3-dev patchelf pkg-config git \
      linux-perf procps && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install 1.93.0 --profile minimal --no-self-update \
      --component rustfmt --component clippy && rustup default 1.93.0
RUN python3 -m venv /opt/build-venv && /opt/build-venv/bin/pip install --no-cache-dir maturin==1.15.0
ENV CARGO_TARGET_DIR=/data/cargo-target CARGO_HOME=/data/cargo-home \
    PATH=/opt/build-venv/bin:/usr/local/cargo/bin:$PATH \
    FZ_WORKBOOKS=/data/workbooks CARGO_BUILD_JOBS=16
WORKDIR /src
COPY . /src
# A worker, not a server: it waits for `railway ssh` sessions.
CMD ["sh", "-c", "mkdir -p /data/cargo-target /data/cargo-home /data/workbooks /data/wheelhouse && exec sleep infinity"]
