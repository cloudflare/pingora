FROM debian:latest as builder

ARG BUILDARCH
RUN apt-get -qq update \
    && apt-get -qq install -y --no-install-recommends \
       gcc g++ libfindbin-libs-perl \
       make cmake libclang-dev git \
       wget curl gnupg ca-certificates lsb-release \
    && wget --no-check-certificate -O - https://openresty.org/package/pubkey.gpg | gpg --dearmor -o /usr/share/keyrings/openresty.gpg \
    && if [ "${BUILDARCH}" = "arm64" ]; then URL="http://openresty.org/package/arm64/debian"; else URL="http://openresty.org/package/debian"; fi \
    && echo "deb [arch=$BUILDARCH signed-by=/usr/share/keyrings/openresty.gpg] ${URL} $(lsb_release -sc) openresty" | tee /etc/apt/sources.list.d/openresty.list > /dev/null \
    && apt-get -qq update \
    && apt-get -qq install -y openresty --no-install-recommends

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /var/opt/pingora
COPY . .
RUN cargo build
