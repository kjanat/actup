FROM gcr.io/distroless/static-debian13:nonroot@sha256:8ce6dd87e9823c5b48dcfb2c8604714ccb351ce59607327bcc4957448da279b5

ARG TARGETARCH
ARG ACTUP_VERSION=dev
ARG ACTUP_REVISION=unknown

LABEL org.opencontainers.image.title="actup" \
      org.opencontainers.image.description="Check, update and SHA-pin GitHub Actions uses: refs" \
      org.opencontainers.image.source="https://github.com/kjanat/actup" \
      org.opencontainers.image.url="https://github.com/kjanat/actup" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.vendor="Kaj Kowalski" \
      org.opencontainers.image.version="${ACTUP_VERSION}" \
      org.opencontainers.image.revision="${ACTUP_REVISION}"

COPY --chmod=0755 docker/${TARGETARCH}/actup /usr/local/bin/actup
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/actup/

WORKDIR /work
ENTRYPOINT ["/usr/local/bin/actup"]
CMD ["--help"]
