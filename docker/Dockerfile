# Runtime: stock PostgreSQL (PG_MAJOR, 13-18; default 16) + the pgbx extension (built in the dev image).
# Per version: docker build -f docker/Dockerfile.dev --build-arg PG_MAJOR=13 -t pgbx-dev:13 .
#              docker build -f docker/Dockerfile --build-arg PG_MAJOR=13 --build-arg DEV_IMAGE=pgbx-dev:13 .
ARG PG_MAJOR=16
ARG DEV_IMAGE=pgbx-dev
FROM ${DEV_IMAGE} AS build
ARG PG_MAJOR
COPY . /src
RUN cargo pgrx package --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
      --no-default-features --features pg${PG_MAJOR} >/dev/null
RUN cargo build --release --manifest-path cli/Cargo.toml >/dev/null

FROM postgres:${PG_MAJOR}-bookworm
ARG PG_MAJOR
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates >/dev/null && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/pgbx-pg${PG_MAJOR}/usr/lib/postgresql/${PG_MAJOR}/lib/pgbx.so /usr/lib/postgresql/${PG_MAJOR}/lib/
COPY --from=build /src/target/release/pgbx-pg${PG_MAJOR}/usr/share/postgresql/${PG_MAJOR}/extension/ /usr/share/postgresql/${PG_MAJOR}/extension/
COPY --from=build /src/cli/target/release/pgbx /usr/local/bin/pgbx
