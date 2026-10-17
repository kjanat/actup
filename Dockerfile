# syntax=docker/dockerfile:1

FROM gcr.io/distroless/static-debian13:nonroot@sha256:e2e927ec666bae08560abb3c55d0659eceabb657f56b6782ab500a9fc7f555e3

ARG TARGETARCH
ARG ACTUP_VERSION=dev
ARG ACTUP_REVISION=unknown

LABEL org.opencontainers.image.title="actup" \
      org.opencontainers.image.description="Check, update and SHA-pin GitHub Actions uses: refs" \
      org.opencontainers.image.source="https://github.com/kjanat/actup-v2" \
      org.opencontainers.image.url="https://github.com/kjanat/actup-v2" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0" \
      org.opencontainers.image.vendor="Kaj Kowalski" \
      org.opencontainers.image.version="${ACTUP_VERSION}" \
      org.opencontainers.image.revision="${ACTUP_REVISION}"

COPY --chmod=0755 docker/${TARGETARCH}/actup /usr/local/bin/actup

WORKDIR /work
ENTRYPOINT ["/usr/local/bin/actup"]
CMD ["--help"]
