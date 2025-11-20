FROM ubuntu:22.04

# Set environment variables to avoid interactive prompts
ENV DEBIAN_FRONTEND=noninteractive
ENV VARNISH_VERSION=8.0.0

# Install build dependencies
RUN apt-get update && apt-get install -y \
    build-essential \
    wget \
    curl \
    ca-certificates \
    pkg-config \
    python3-docutils \
    python3-sphinx \
    libedit-dev \
    libncurses-dev \
    libpcre2-dev \
    libjemalloc-dev \
    automake \
    autotools-dev \
    libtool \
    libclang-dev \
    clang \
    && rm -rf /var/lib/apt/lists/*

# Download and build Varnish
WORKDIR /tmp/varnish
RUN wget https://varnish-cache.org/_downloads/varnish-${VARNISH_VERSION}.tgz && \
    tar -zxf varnish-${VARNISH_VERSION}.tgz && \
    cd varnish-${VARNISH_VERSION}/ && \
    ./autogen.sh && \
    ./configure --prefix=/usr && \
    make -sj$(nproc) && \
    make install && \
    cd / && \
    rm -rf /tmp/varnish

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
ENV CARGO_HOME=/usr/local/cargo

# Create directory for vmod
WORKDIR /app

# Copy project files
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
COPY tests ./tests

# Build the vmod
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release


# Find the correct vmod directory and install the vmod
RUN VMOD_DIR=$(pkg-config --variable=vmoddir varnishapi) && \
    mkdir -p ${VMOD_DIR} && \
    cp target/release/libvmod_lambda.so ${VMOD_DIR}/libvmod_lambda.so && \
    find target/release -name '*.vcc' | head -1 | xargs -I {} cp {} ${VMOD_DIR}/vmod_lambda.vcc && \
    ls -la ${VMOD_DIR}/ && \
    echo "Installed vmod to ${VMOD_DIR}"

# Expose Varnish default port
EXPOSE 6081

# Set default command to run Varnish
CMD ["varnishd", "-F", "-f", "/etc/varnish/default.vcl", "-a", ":6081"]
