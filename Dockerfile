FROM alpine:3.21

ARG VERSION=latest
ARG TARGETARCH

SHELL ["/bin/sh", "-euo", "pipefail", "-c"]

RUN apk add --no-cache ca-certificates wget \
    && case "${TARGETARCH}" in \
         amd64) TARGET="x86_64-unknown-linux-musl" ;; \
         arm)   TARGET="arm-unknown-linux-musleabihf" ;; \
         *)     echo "ERROR: Unsupported architecture: ${TARGETARCH}. Only amd64 and 32-bit arm are supported. arm64/aarch64 support planned for Phase 2." >&2; exit 1 ;; \
       esac \
    && if [ "${VERSION}" = "latest" ]; then \
         VERSION_TAG=$(wget -qO- "https://api.github.com/repos/yinqiwen/rsnova/releases/latest" \
           | grep '"tag_name"' | head -1 | cut -d '"' -f 4) \
         && [ -n "${VERSION_TAG}" ] || { echo "ERROR: Failed to resolve latest release tag. Check network or use VERSION=vX.Y.Z to avoid API calls." >&2; exit 1; }; \
       else \
         VERSION_TAG="${VERSION}"; \
       fi \
    && NUMERIC_VERSION="${VERSION_TAG#v}" \
    && DOWNLOAD_URL="https://github.com/yinqiwen/rsnova/releases/download/${VERSION_TAG}/rsnova-${NUMERIC_VERSION}-${TARGET}.tar.gz" \
    && echo "Downloading: ${DOWNLOAD_URL}" \
    && wget -qO /tmp/rsnova.tar.gz "${DOWNLOAD_URL}" \
    || { echo "ERROR: Download failed. Verify release ${VERSION_TAG} exists and contains ${TARGET} artifact." >&2; exit 1; } \
    && tar xzf /tmp/rsnova.tar.gz -C /usr/local/bin/ \
    && rm /tmp/rsnova.tar.gz \
    && chmod +x /usr/local/bin/rsnova

ENTRYPOINT ["rsnova"]
