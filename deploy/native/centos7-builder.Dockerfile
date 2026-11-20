FROM quay.io/centos/centos:7.9.2009@sha256:e4ca2ed0202e76be184e75fb26d14bf974193579039d5573fb2348664deef76e

RUN sed -i 's|^mirrorlist=|#mirrorlist=|g; s|^#baseurl=http://mirror.centos.org|baseurl=http://vault.centos.org|g' /etc/yum.repos.d/CentOS-*.repo \
 && yum install -y gcc gcc-c++ make cmake perl perl-IPC-Cmd perl-Time-Piece curl ca-certificates git tar gzip \
 && yum clean all \
 && rm -rf /var/cache/yum

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain none --profile minimal \
 && /root/.cargo/bin/rustup toolchain install 1.98.1 --profile minimal --no-self-update

ENV PATH=/root/.rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin:/root/.cargo/bin:$PATH
ENV RUSTUP_TOOLCHAIN=1.98.1-x86_64-unknown-linux-gnu
ENV SQLX_OFFLINE=true

WORKDIR /src
COPY . .
RUN /root/.rustup/toolchains/1.98.1-x86_64-unknown-linux-gnu/bin/cargo build --release --locked -p tgm
