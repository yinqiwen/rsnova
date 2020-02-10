FROM alpine:3.11

ARG TZ="Asia/Shanghai"
ARG RSNOVA_VER="v0.2.0"
ARG RMUX_CIPHER_KEY="abcdefghijk"
ARG WS_CIPHER_KEY="abcdefghijk"

ENV TZ ${TZ}
ENV RSNOVA_VER ${RSNOVA_VER}
ENV RSNOVA_DOWNLOAD_URL https://github.com/yinqiwen/rsnova/releases/download/${RSNOVA_VER}/rsnova-${RSNOVA_VER}-x86_64-unknown-linux-musl.tar.gz

RUN apk upgrade --update \
    && apk add curl tzdata \
    && mkdir rsnova \
    && (cd rsnova && curl -sfSL ${RSNOVA_DOWNLOAD_URL} | tar xz) \
    && mv /rsnova/rsnova /usr/bin/rsnova \
    && mv /rsnova /etc/rsnova \
    && chmod +x /usr/bin/rsnova \
    && ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime \
    && echo ${TZ} > /etc/timezone \
    && apk del curl \
    && rm -rf /var/cache/apk/*

RUN   sed -i "s|\${RMUX_CIPHER_KEY}|${RMUX_CIPHER_KEY}|g" /etc/rsnova/server.toml
RUN   sed -i "s|\${WS_CIPHER_KEY}|${WS_CIPHER_KEY}|g" /etc/rsnova/server.toml

# Document that the service listens on port 48101/48102.
EXPOSE 48101 48102 

# Run the outyet command by default when the container starts.
CMD ["rsnova","-c" ,"/etc/rsnova/server.toml"]
