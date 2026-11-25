# Base review: Alpine 3.24.4, linux/amd64 manifest pinned by digest.
# 3.24 is the current stable branch; 3.20 is past end of support.
# No apk packages in either stage: BusyBox supplies wget, sha256sum, tar, and
# grep, and the static ntfy binary needs neither ca-certificates (no outbound
# TLS, empty upstream-base-url) nor tzdata (UTC logs).
FROM alpine:3.24.4@sha256:0f49f331d9212aff11b932863be4eb01cdf962969aa86dab7d1ee21ac6f5f52b AS fetch

# The release binary is fetched and checked in the build, so the build context
# holds no pre-downloaded file. Version, architecture, and checksum are pinned;
# the checksum is the linux/amd64 value from the release checksums.txt. The base
# digest above is the amd64 manifest, so amd64 is the claimed architecture;
# another one needs its own base digest and release checksum.
ARG NTFY_VERSION=2.38.2
ARG NTFY_ARCH=amd64
ARG NTFY_SHA256=2040946f6e1103420e72a79a7048c841fb73cd51c17d45232e0c8f6ac7f0d531

RUN set -eux; \
    wget -q -O /tmp/ntfy.tgz "https://github.com/binwiederhier/ntfy/releases/download/v${NTFY_VERSION}/ntfy_${NTFY_VERSION}_linux_${NTFY_ARCH}.tar.gz"; \
    echo "${NTFY_SHA256}  /tmp/ntfy.tgz" | sha256sum -c -; \
    tar -xzf /tmp/ntfy.tgz -C /tmp --strip-components=1 "ntfy_${NTFY_VERSION}_linux_${NTFY_ARCH}/ntfy"

FROM alpine:3.24.4@sha256:0f49f331d9212aff11b932863be4eb01cdf962969aa86dab7d1ee21ac6f5f52b

# COPY --chmod avoids the second 76.6 MB layer that a separate chmod RUN created
# in the 166 MB alpine:3.20 image.
COPY --from=fetch --chmod=0755 /tmp/ntfy /usr/bin/ntfy
COPY server.yml /etc/ntfy/server.yml

RUN mkdir -p /var/cache/ntfy/attachments \
 && chown -R 1000:1000 /var/cache/ntfy

# Rootless Podman already maps container root to an unprivileged subuid; a named
# user is defense in depth. An existing cache volume must be chowned once.
USER 1000:1000
VOLUME /var/cache/ntfy
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
  CMD wget -q -O - http://127.0.0.1:8080/v1/health | grep -q '"healthy":true' || exit 1

ENTRYPOINT ["ntfy"]
CMD ["serve"]
