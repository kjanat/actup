FROM gcr.io/distroless/static-debian13:nonroot@sha256:25ac8f0c3b2334a9eb05452266bd4697864a8c8ef0ef71e5e6480d7f92868329

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
