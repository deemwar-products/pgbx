//! pgbx — zero-touch Postgres backups to S3, controlled from SQL.
//!
//! Loaded via `shared_preload_libraries`, it registers server-wide settings (`pgbx.*`) and one
//! background worker. The worker installs the extension into every database (and `template1`, so new
//! databases are born with it), backs each one up on its schedule, and runs queued manual backups/restores.
//! Per-database policy lives in `pgbx.config`; every run is recorded in `pgbx.history`.

use pgrx::bgworkers::BackgroundWorkerBuilder;
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use std::ffi::CString;
use std::time::Duration;

mod audit;
mod schedule;
mod transfer;
mod worker;

/// 'every 1 hour' / 'daily at 02:30' / cron -> cron. Errors (with the accepted forms) if it can't be read.
#[pg_extern(immutable, strict)]
fn to_cron(schedule: &str) -> String {
    schedule::to_cron(schedule).unwrap_or_else(|e| pgrx::error!("pgbx: {e}"))
}

/// Internal: presign a GET for one key. Execute is revoked from PUBLIC; use pgbx.download_url().
#[pg_extern(strict, name = "_presign")]
fn presign(key: &str, expires_seconds: i32) -> String {
    let secs = expires_seconds.clamp(60, 604_800) as u32; // S3 allows at most 7 days
    let b = worker::bucket().unwrap_or_else(|e| pgrx::error!("pgbx: {e}"));
    b.presign_get(key, secs, None).unwrap_or_else(|e| pgrx::error!("pgbx: presign failed: {e}"))
}

/// Next run (epoch seconds) of a cron schedule strictly after `after_epoch`; used by status().
#[pg_extern(immutable, strict)]
fn next_run_epoch(cron: &str, after_epoch: f64) -> f64 {
    schedule::next_after_epoch(cron, after_epoch).unwrap_or_else(|e| pgrx::error!("pgbx: {e}"))
}

::pgrx::pg_module_magic!(name, version);

// ---- server-wide settings (postgresql.conf; change + `SELECT pg_reload_conf()`) ----
pub static S3_ENDPOINT: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static S3_BUCKET: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static S3_REGION: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"us-east-1"));
pub static SERVER_NAME: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static CREDENTIALS_FILE: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"/etc/pgbx/s3.credentials"));
pub static SOCKET_DIR: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"/var/run/postgresql"));
pub static POLL_SECONDS: GucSetting<i32> = GucSetting::<i32>::new(5);
pub static MAX_DAYS_LIMIT: GucSetting<i32> = GucSetting::<i32>::new(90);
// alerts: run on every failed job (JSON on stdin, PGBX_* env vars)
pub static ALERT_COMMAND: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static DUMP_COMPRESSION: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"auto"));
pub static AUDIT_DAYS: GucSetting<i32> = GucSetting::<i32>::new(30);
pub static ADMIN_DB: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"postgres"));
// bandwidth caps per job (ADR 0001 §3)
pub static UPLOAD_KBPS: GucSetting<i32> = GucSetting::<i32>::new(0);
pub static DOWNLOAD_KBPS: GucSetting<i32> = GucSetting::<i32>::new(0);
// resource caps on pg_dump / pg_restore (ADR 0001 §3)
pub static JOB_NICE: GucSetting<i32> = GucSetting::<i32>::new(10);
pub static JOB_IONICE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"best-effort-7"));
pub static DUMP_COMPRESSION_BUSY: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"auto"));
pub static DUMP_LOCK_TIMEOUT: GucSetting<i32> = GucSetting::<i32>::new(5_000); // ms
pub static DUMP_LOCK_TIMEOUT_FORCED: GucSetting<i32> = GucSetting::<i32>::new(60_000); // ms
pub static RESTORE_SYNCHRONOUS_COMMIT: GucSetting<bool> = GucSetting::<bool>::new(false);
pub static DOCTOR_LONG_JOB: GucSetting<i32> = GucSetting::<i32>::new(3600); // s
// deferring a backup (lock timeout now, the load gate later): backoff and the deadline it never passes
pub static DEFER_BACKOFF: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(Some(c"1,2,4,8,15"));
pub static MAX_DEFER: GucSetting<i32> = GucSetting::<i32>::new(4 * 3600); // s
pub static MAX_DEFER_FIRST: GucSetting<i32> = GucSetting::<i32>::new(15 * 60); // s
pub static COALESCE_MANUAL: GucSetting<bool> = GucSetting::<bool>::new(true);
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_string_guc(c"pgbx.s3_endpoint", c"S3 endpoint URL", c"e.g. https://hel1.your-objectstorage.com", &S3_ENDPOINT, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.s3_bucket", c"S3 bucket for all backups of this server", c"", &S3_BUCKET, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.s3_region", c"S3 region", c"", &S3_REGION, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.server_name", c"Top-level folder for this server in the bucket", c"defaults to the hostname", &SERVER_NAME, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.credentials_file", c"File with access_key_id= and secret_access_key= lines", c"keep it readable by the postgres OS user only", &CREDENTIALS_FILE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.socket_dir", c"Unix socket directory the worker connects through", c"", &SOCKET_DIR, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.max_days_limit", c"Ceiling on any database's max_days", c"stops a database from keeping backups longer than the server allows", &MAX_DAYS_LIMIT, 1, 36500, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.alert_command", c"Shell command run for every failed job", c"gets JSON on stdin and PGBX_DATABASE/PGBX_KIND/PGBX_JOB_ID/PGBX_ERROR/PGBX_SERVER env vars", &ALERT_COMMAND, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.dump_compression", c"pg_dump --compress for per-database backups", c"auto (default: zstd:3 with pg_dump 16+, gzip level 6 before), or e.g. zstd:3, lz4, gzip:6, 6, none", &DUMP_COMPRESSION, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.admin_db", c"Database that holds the server-wide overview and doctor()", c"", &ADMIN_DB, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.audit_days", c"Days of history (audit trail) kept per database", c"older rows are pruned, except kept backups, open incidents/gaps and the newest row of each kind", &AUDIT_DAYS, 1, 36500, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.poll_seconds", c"How often the worker looks for new databases and due/queued jobs", c"", &POLL_SECONDS, 1, 3600, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.upload_kbps", c"Upload bandwidth cap per job in KiB/s", c"0 = unlimited", &UPLOAD_KBPS, 0, 10_000_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.download_kbps", c"Download (restore) bandwidth cap per job in KiB/s", c"0 = unlimited", &DOWNLOAD_KBPS, 0, 10_000_000, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.job_nice", c"CPU niceness of pg_dump / pg_restore", c"0-19; never raises priority above the worker's own", &JOB_NICE, 0, 19, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.job_ionice", c"IO priority of pg_dump / pg_restore (Linux)", c"none, idle, or best-effort-0 .. best-effort-7; anything else means best-effort-7", &JOB_IONICE, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_string_guc(c"pgbx.dump_compression_busy", c"pg_dump --compress for a backup forced to run while the server is busy", c"auto (default: zstd:1 with pg_dump 16+, gzip level 1 before; never more than pgbx.dump_compression), or a pg_dump --compress value", &DUMP_COMPRESSION_BUSY, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.dump_lock_timeout", c"How long pg_dump waits for its table locks before the backup is retried later", c"0 = wait forever; never queue behind DDL", &DUMP_LOCK_TIMEOUT, 0, 600_000, GucContext::Sighup, GucFlags::UNIT_MS);
    GucRegistry::define_int_guc(c"pgbx.dump_lock_timeout_forced", c"pg_dump lock wait once a deferred backup reached its deadline", c"if it still times out, the backup fails and alerts", &DUMP_LOCK_TIMEOUT_FORCED, 0, 600_000, GucContext::Sighup, GucFlags::UNIT_MS);
    GucRegistry::define_bool_guc(c"pgbx.restore_synchronous_commit", c"synchronous_commit for pg_restore", c"off by default: the target is a new database, a crash just means restoring again", &RESTORE_SYNCHRONOUS_COMMIT, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.doctor_long_job", c"doctor() warns about a backup/restore process running longer than this", c"0 = never", &DOCTOR_LONG_JOB, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_string_guc(c"pgbx.defer_backoff", c"Minutes between retries of a deferred backup", c"comma list, each 1-60; the last value repeats", &DEFER_BACKOFF, GucContext::Sighup, GucFlags::default());
    GucRegistry::define_int_guc(c"pgbx.max_defer", c"A deferred backup runs anyway this long after it was queued", c"capped at the schedule interval; backups are never skipped", &MAX_DEFER, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_int_guc(c"pgbx.max_defer_first", c"max_defer for a new database's first backup", c"", &MAX_DEFER_FIRST, 0, 86_400, GucContext::Sighup, GucFlags::UNIT_S);
    GucRegistry::define_bool_guc(c"pgbx.coalesce_manual", c"backup_now() / verify_now() return the job already queued instead of adding another", c"", &COALESCE_MANUAL, GucContext::Sighup, GucFlags::default());

    // Only register the worker when loaded at server start (shared_preload_libraries),
    // not when a backend loads the library for CREATE EXTENSION.
    if unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        BackgroundWorkerBuilder::new("pgbx scheduler")
            .set_function("pgbx_worker_main")
            .set_library("pgbx")
            .set_restart_time(Some(Duration::from_secs(10)))
            .load();
    }
}

extension_sql!(
    r#"
-- Access roles (server-wide; created once, reused by every database). Nobody else gets anything.
--   pgbx_viewer: status(), backups      pgbx_admin: manage this database's backups (+ viewer)
DO $roles$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pgbx_viewer') THEN
        CREATE ROLE pgbx_viewer NOLOGIN;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'pgbx_admin') THEN
        CREATE ROLE pgbx_admin NOLOGIN IN ROLE pgbx_viewer;
    END IF;
END $roles$;

-- Server-wide summary, one row per database, written by the worker; read via overview() in the admin database.
CREATE TABLE pgbx.server_overview (
    database         name PRIMARY KEY,
    state            text,
    schedule         text,
    last_backup_at   timestamptz,
    last_backup_size text,
    next_backup_at   timestamptz,
    backups_kept     bigint,
    last_verify      text,
    last_error       text,
    seen_at          timestamptz NOT NULL DEFAULT now()
);

-- One row per database. path NULL = use the database name (so the row copied from template1 stays correct).
CREATE TABLE pgbx.config (
    id             int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    path           text,
    schedule       text NOT NULL DEFAULT '0 2 * * *',          -- cron (normalized)
    schedule_label text NOT NULL DEFAULT 'daily at 02:00',     -- as the user wrote it
    max_backups    int  NOT NULL DEFAULT 14 CHECK (max_backups >= 1),   -- keep at most this many backups
    max_days       int  NOT NULL DEFAULT 90 CHECK (max_days >= 1),      -- and none older than this (newest always kept)
    verify_schedule text DEFAULT '0 4 * * 0',                  -- restore test; NULL = never
    verify_label   text NOT NULL DEFAULT 'weekly on sunday at 04:00',
    enabled        bool NOT NULL DEFAULT true,                 -- false = paused
    paused_reason  text,
    paused_at      timestamptz,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    -- data scope: every table's DEFINITION is always backed up; these only decide whose ROWS are kept
    include_data   text[],                                     -- NULL/empty = rows of all tables
    exclude_data   text[]                                      -- rows of these are skipped
);
-- No default row here: a restored dump brings its own row, and a fresh database gets one from the
-- worker or configure() (INSERT ... ON CONFLICT DO NOTHING), so the two never collide.

-- Every backup / restore, queued manual or automatic. The worker claims rows in state 'queued'.
CREATE TABLE pgbx.history (
    id           bigserial PRIMARY KEY,
    kind         text NOT NULL CHECK (kind IN ('backup', 'restore', 'config', 'pause', 'resume', 'prune', 'verify')),
    trigger      text NOT NULL DEFAULT 'manual' CHECK (trigger IN ('manual', 'schedule', 'first', 'migration')),
    params       jsonb NOT NULL DEFAULT '{}',
    state        text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running', 'done', 'failed', 'expired', 'cancelled')),
    requested_at timestamptz NOT NULL DEFAULT now(),
    started      timestamptz,
    finished     timestamptz,
    s3_key       text,
    bytes        bigint,
    error        text,
    who          text DEFAULT session_user                     -- the role that asked (worker rows: the worker's role)
);
CREATE INDEX ON pgbx.history (state) WHERE state IN ('queued', 'running');

-- keep policy + history in logical dumps of this database
SELECT pg_catalog.pg_extension_config_dump('pgbx.config', '');
SELECT pg_catalog.pg_extension_config_dump('pgbx.history', '');

-- internal: record a policy change in history
CREATE FUNCTION pgbx._log(kind text, params jsonb) RETURNS void LANGUAGE sql AS $$
    INSERT INTO pgbx.history (kind, trigger, params, state, started, finished)
    VALUES (kind, 'migration', params, 'done', now(), now())
$$;

-- Change this database's policy. NULL arguments leave a setting unchanged. Meant for migrations.
-- schedule accepts the same forms as set_schedule().
CREATE FUNCTION pgbx.configure(
    schedule text DEFAULT NULL, max_backups int DEFAULT NULL, max_days int DEFAULT NULL,
    enabled bool DEFAULT NULL, path text DEFAULT NULL
) RETURNS pgbx.config LANGUAGE plpgsql AS $$
DECLARE r pgbx.config; c text := CASE WHEN schedule IS NULL THEN NULL ELSE pgbx.to_cron(schedule) END;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config x SET
        schedule       = coalesce(c, x.schedule),
        schedule_label = coalesce(configure.schedule, x.schedule_label),
        max_backups    = coalesce(configure.max_backups, x.max_backups),
        max_days       = coalesce(pgbx._check_days(configure.max_days), x.max_days),
        enabled        = coalesce(configure.enabled, x.enabled),
        paused_at      = CASE WHEN configure.enabled IS NULL THEN x.paused_at WHEN configure.enabled THEN NULL ELSE now() END,
        paused_reason  = CASE WHEN configure.enabled IS NULL THEN x.paused_reason WHEN configure.enabled THEN NULL ELSE 'configure(enabled => false)' END,
        path           = coalesce(configure.path, x.path),
        updated_at     = now()
    RETURNING * INTO r;
    PERFORM pgbx._log('config', to_jsonb(r));
    IF configure.max_backups IS NOT NULL OR configure.max_days IS NOT NULL THEN
        INSERT INTO pgbx.history (kind, trigger) VALUES ('prune', 'migration');
    END IF;
    RETURN r;
END $$;

-- internal: enforce the server-wide ceiling pgbx.max_days_limit
CREATE FUNCTION pgbx._check_days(d int) RETURNS int LANGUAGE plpgsql AS $$
DECLARE lim int := coalesce(nullif(current_setting('pgbx.max_days_limit', true), '')::int, 90);
BEGIN
    IF d IS NOT NULL AND d > lim THEN
        RAISE EXCEPTION 'pgbx: max_days % exceeds this server''s limit of % days (pgbx.max_days_limit)', d, lim;
    END IF;
    RETURN d;
END $$;

-- Expiry: keep at most max_backups backups AND none older than max_days (whichever deletes first;
-- the newest backup is always kept). Takes effect on the worker's next pass.
CREATE FUNCTION pgbx.set_retention(max_backups int DEFAULT NULL, max_days int DEFAULT NULL) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE r pgbx.config;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config x SET
        max_backups = coalesce(set_retention.max_backups, x.max_backups),
        max_days    = coalesce(pgbx._check_days(set_retention.max_days), x.max_days),
        updated_at  = now()
    RETURNING * INTO r;
    PERFORM pgbx._log('config', jsonb_build_object('max_backups', r.max_backups, 'max_days', r.max_days));
    INSERT INTO pgbx.history (kind, trigger) VALUES ('prune', 'manual');
    RETURN format('keeping at most %s backups and nothing older than %s days (newest always kept); pruning now',
                  r.max_backups, r.max_days);
END $$;

-- 'every 1 hour', 'every 15 minutes', 'daily at 02:30', 'weekly on sunday at 03:00', 'hourly', or cron.
-- Validated immediately; returns a sentence saying when the next backup will run.
CREATE FUNCTION pgbx.set_schedule(schedule text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE c text := pgbx.to_cron(schedule);
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET schedule = c, schedule_label = set_schedule.schedule, updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('schedule', set_schedule.schedule, 'cron', c));
    RETURN format('schedule set to "%s" (cron %s); next backup at %s', schedule, c,
                  to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now()))));
END $$;

-- Stop automatic backups (manual backup_now()/restore() keep working). The reason shows in status().
CREATE FUNCTION pgbx.pause(reason text DEFAULT NULL) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET enabled = false, paused_at = now(), paused_reason = reason, updated_at = now();
    PERFORM pgbx._log('pause', jsonb_build_object('reason', reason));
    RETURN 'automatic backups paused' || coalesce(' (' || reason || ')', '') || '; resume with SELECT pgbx.resume()';
END $$;

CREATE FUNCTION pgbx.resume() RETURNS text LANGUAGE plpgsql AS $$
DECLARE c text;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET enabled = true, paused_at = NULL, paused_reason = NULL, updated_at = now() RETURNING schedule INTO c;
    PERFORM pgbx._log('resume', '{}');
    RETURN format('automatic backups resumed; next backup at %s', to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now()))));
END $$;

-- Every completed backup of this database, newest first.
CREATE VIEW pgbx.backups AS
    SELECT id, finished AS taken_at, now() - finished AS age, trigger, pg_size_pretty(bytes) AS size, bytes, s3_key
    FROM pgbx.history WHERE kind = 'backup' AND state = 'done' ORDER BY id DESC;

-- One row answering "are we backed up?": state, schedule, last + next backup, last error, where it goes.
CREATE FUNCTION pgbx.status() RETURNS TABLE (
    database name, state text, schedule text, cron text, next_backup_at timestamptz,
    last_backup_at timestamptz, last_backup_age interval, last_backup_size text, last_backup_key text,
    backups_kept bigint, retention text, data_scope text, verify_schedule text, last_verified_at timestamptz, last_verify_result text,
    last_error text, last_error_at timestamptz, paused_reason text, paused_at timestamptz,
    queued_jobs bigint, location text
) LANGUAGE plpgsql STABLE AS $$
DECLARE cfg pgbx.config; lb pgbx.history; le pgbx.history; lv pgbx.history; last_auto timestamptz;
BEGIN
    SELECT * INTO cfg FROM pgbx.config;
    IF NOT FOUND THEN  -- worker hasn't visited this database yet
        cfg := ROW(1, NULL, '0 2 * * *', 'daily at 02:00', 14, 90, '0 4 * * 0', 'weekly on sunday at 04:00',
                   true, NULL, NULL, now(), NULL, NULL)::pgbx.config;
    END IF;
    SELECT * INTO lb FROM pgbx.history WHERE kind='backup' AND history.state='done' ORDER BY id DESC LIMIT 1;
    SELECT * INTO le FROM pgbx.history WHERE history.state='failed' ORDER BY id DESC LIMIT 1;
    SELECT * INTO lv FROM pgbx.history WHERE kind='verify' AND history.state IN ('done','failed') ORDER BY id DESC LIMIT 1;
    SELECT max(requested_at) INTO last_auto FROM pgbx.history WHERE kind='backup' AND trigger IN ('schedule','first');
    RETURN QUERY SELECT
        current_database()::name,
        CASE WHEN NOT cfg.enabled THEN 'paused'
             WHEN EXISTS (SELECT 1 FROM pgbx.history h WHERE h.state='running') THEN 'running'
             WHEN lb.id IS NULL THEN 'waiting for first backup'
             WHEN le.id > lb.id THEN 'failing'
             ELSE 'active' END,
        cfg.schedule_label, cfg.schedule,
        CASE WHEN NOT cfg.enabled THEN NULL
             WHEN last_auto IS NULL THEN now()
             ELSE to_timestamp(pgbx.next_run_epoch(cfg.schedule, extract(epoch FROM last_auto))) END,
        lb.finished, now() - lb.finished, pg_size_pretty(lb.bytes), lb.s3_key,
        (SELECT count(*) FROM pgbx.history h WHERE h.kind='backup' AND h.state='done'),
        format('max %s backups, max %s days', cfg.max_backups, cfg.max_days),
        CASE WHEN coalesce(cardinality(cfg.include_data), 0) = 0 AND coalesce(cardinality(cfg.exclude_data), 0) = 0
             THEN 'all tables, all rows'
             ELSE 'all tables; rows of ' ||
                  CASE WHEN coalesce(cardinality(cfg.include_data), 0) > 0 THEN 'only ' || array_to_string(cfg.include_data, ', ')
                       ELSE 'all tables' END ||
                  CASE WHEN coalesce(cardinality(cfg.exclude_data), 0) > 0 THEN ' except ' || array_to_string(cfg.exclude_data, ', ')
                       ELSE '' END ||
                  format(' (%s tables backed up without rows)', (SELECT count(*) FROM pgbx.rowless_tables()))
        END,
        coalesce(cfg.verify_label, 'never'), lv.finished,
        CASE WHEN lv.id IS NULL THEN NULL WHEN lv.state = 'done' THEN 'ok: ' || coalesce(lv.params->>'checked', '') ELSE 'FAILED: ' || lv.error END,
        le.error, le.finished, cfg.paused_reason, cfg.paused_at,
        (SELECT count(*) FROM pgbx.history h WHERE h.state='queued'),
        format('s3://%s/%s/%s/', current_setting('pgbx.s3_bucket', true),
               coalesce(nullif(current_setting('pgbx.server_name', true), ''), '<hostname>'),
               coalesce(cfg.path, current_database()));
END $$;

-- internal: queue a manual job, or (pgbx.coalesce_manual, default on) return the one of that kind already queued
-- in this database
CREATE FUNCTION pgbx._queue_manual(k text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE j bigint;
BEGIN
    IF coalesce(current_setting('pgbx.coalesce_manual', true), 'on') <> 'off' THEN
        PERFORM pg_advisory_xact_lock(hashtext('pgbx_coalesce'), hashtext(k));
        SELECT id INTO j FROM pgbx.history WHERE kind = k AND state = 'queued' ORDER BY id LIMIT 1;
        IF j IS NOT NULL THEN
            UPDATE pgbx.history SET params = params || jsonb_build_object('manual', true,
                       'coalesced', coalesce((params->>'coalesced')::int, 0) + 1)
             WHERE id = j;
            RAISE NOTICE 'pgbx: % job % is already queued; returning it instead of adding another', k, j;
            RETURN j;
        END IF;
    END IF;
    INSERT INTO pgbx.history (kind, trigger) VALUES (k, 'manual') RETURNING id INTO j;
    RETURN j;
END $$;

-- Cancel a queued job of this database: it never starts and ends as 'cancelled'.
CREATE FUNCTION pgbx.cancel(job_id bigint) RETURNS text LANGUAGE plpgsql AS $$
DECLARE h pgbx.history;
BEGIN
    SELECT * INTO h FROM pgbx.history WHERE id = job_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'pgbx: no job % in database %', job_id, current_database();
    ELSIF h.state = 'queued' THEN
        UPDATE pgbx.history SET state = 'cancelled', finished = now(), error = 'cancelled by ' || session_user WHERE id = job_id;
        RETURN format('%s job %s cancelled before it started', h.kind, job_id);
    END IF;
    RAISE EXCEPTION 'pgbx: job % is % (only a queued job can be cancelled)', job_id, h.state;
END $$;

-- Restore test: restore the newest backup into a scratch database, check it, drop it. Like backup_now(), returns
-- the restore test already queued (pgbx.coalesce_manual) instead of adding another.
CREATE FUNCTION pgbx.verify_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('verify');
END $$;

-- 'weekly on sunday at 04:00', 'daily at 05:00', ... or 'never'.
CREATE FUNCTION pgbx.set_verify_schedule(schedule text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE s text := set_verify_schedule.schedule;
        c text := CASE WHEN lower(trim(set_verify_schedule.schedule)) IN ('never', 'off') THEN NULL
                       ELSE pgbx.to_cron(set_verify_schedule.schedule) END;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET verify_schedule = c, verify_label = CASE WHEN c IS NULL THEN 'never' ELSE s END, updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('verify_schedule', s, 'cron', c));
    RETURN CASE WHEN c IS NULL THEN 'restore tests disabled'
           ELSE format('restore test "%s" (cron %s); next at %s', s, c,
                       to_timestamp(pgbx.next_run_epoch(c, extract(epoch FROM now())))) END;
END $$;

-- internal: server-wide functions (overview, doctor) run in the admin database (pgbx.admin_db, default 'postgres')
CREATE FUNCTION pgbx._require_admin_db() RETURNS void LANGUAGE plpgsql AS $$
DECLARE a text := coalesce(nullif(current_setting('pgbx.admin_db', true), ''), 'postgres');
BEGIN
    IF current_database() <> a THEN
        RAISE EXCEPTION 'pgbx: server-wide functions run in database "%" (pgbx.admin_db); connect there', a;
    END IF;
END $$;

-- A time-limited link to download one backup of THIS database (newest if backup_id is NULL) — no bucket keys needed:
--   curl -o shop.dump "$url"   or   curl -s "$url" | pg_restore -d some_db
-- Not open to PUBLIC: GRANT EXECUTE ON FUNCTION pgbx.download_url(bigint, interval) TO <role>;
CREATE FUNCTION pgbx.download_url(backup_id bigint DEFAULT NULL, expires interval DEFAULT '1 hour')
RETURNS text LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pgbx AS $$
DECLARE k text; secs int := extract(epoch FROM expires)::int;
BEGIN
    IF secs < 60 OR secs > 604800 THEN
        RAISE EXCEPTION 'pgbx: expires must be between 1 minute and 7 days';
    END IF;
    SELECT s3_key INTO k FROM pgbx.history
     WHERE kind = 'backup' AND state = 'done' AND s3_key IS NOT NULL AND (backup_id IS NULL OR id = backup_id)
     ORDER BY id DESC LIMIT 1;
    IF k IS NULL THEN
        RAISE EXCEPTION 'pgbx: no such backup in this database%', coalesce(' (id ' || backup_id || ')', '');
    END IF;
    PERFORM pgbx._log('config', jsonb_build_object('download_url', k, 'by', session_user, 'expires', expires));
    RETURN pgbx._presign(k, secs);
END $$;
REVOKE ALL ON FUNCTION pgbx.download_url(bigint, interval) FROM PUBLIC;

-- Every database on this server in one view ("is the whole server backed up?"). Admin database only.
CREATE FUNCTION pgbx.overview() RETURNS TABLE (
    database name, state text, schedule text, last_backup_at timestamptz, last_backup_age interval,
    last_backup_size text, next_backup_at timestamptz, backups_kept bigint, last_verify text, last_error text, seen_at timestamptz
) LANGUAGE plpgsql STABLE AS $$
BEGIN
    PERFORM pgbx._require_admin_db();
    RETURN QUERY SELECT o.database, o.state, o.schedule, o.last_backup_at, now() - o.last_backup_at, o.last_backup_size,
                        o.next_backup_at, o.backups_kept, o.last_verify, o.last_error, o.seen_at
                 FROM pgbx.server_overview o ORDER BY o.database;
END $$;

-- internal: how old a backup on this schedule may get before it counts as overdue: 2x the longest gap
-- between the next few runs + 1 hour. NULL when the schedule cannot be read.
CREATE FUNCTION pgbx._overdue_after(schedule text) RETURNS interval LANGUAGE plpgsql STABLE AS $$
DECLARE c text; t float8; prev float8; gap float8 := 0;
BEGIN
    c := pgbx.to_cron(schedule);
    prev := pgbx.next_run_epoch(c, extract(epoch FROM now())::float8);
    FOR i IN 1..3 LOOP
        t := pgbx.next_run_epoch(c, prev);
        gap := greatest(gap, t - prev);
        prev := t;
    END LOOP;
    RETURN make_interval(secs => 2 * gap) + interval '1 hour';
EXCEPTION WHEN others THEN
    RETURN NULL;
END $$;

-- internal: strip anything that could carry a secret from text shown by doctor()
CREATE FUNCTION pgbx._scrub(t text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT regexp_replace(regexp_replace(t,
        '(x-amz-[a-z0-9-]*|[a-z0-9_-]*(key|secret)[a-z0-9_-]*)\s*[=:]\s*\S+', '\1=<redacted>', 'gi'),
        '[A-Za-z0-9/+]{32,}', '<redacted>', 'g')
$$;

-- internal: the checks behind doctor(); nothing here quotes the credentials file
CREATE FUNCTION pgbx._doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
LANGUAGE plpgsql STABLE AS $$
DECLARE
    o record; v text; lim interval; n int; bad text; worst_ratio float8 := -1; su bool;
BEGIN
    PERFORM pgbx._require_admin_db();
    su := coalesce((SELECT r.rolsuper FROM pg_roles r WHERE r.rolname = session_user), false);

    v := current_setting('shared_preload_libraries', true);
    name := 'extension loaded'; ok := v ~ '(^|[ ,])pgbx($|[ ,])';
    detail := CASE WHEN ok THEN 'pgbx is in shared_preload_libraries' ELSE 'pgbx is not in shared_preload_libraries, so no background worker runs' END;
    fix := CASE WHEN ok THEN NULL ELSE 'add pgbx to shared_preload_libraries in postgresql.conf and restart Postgres' END;
    RETURN NEXT;

    name := 's3 settings';
    v := concat_ws(', ', CASE WHEN coalesce(current_setting('pgbx.s3_endpoint', true), '') = '' THEN 'pgbx.s3_endpoint' END,
                         CASE WHEN coalesce(current_setting('pgbx.s3_bucket', true), '') = '' THEN 'pgbx.s3_bucket' END,
                         CASE WHEN coalesce(current_setting('pgbx.credentials_file', true), '') = '' THEN 'pgbx.credentials_file' END);
    ok := v = '';
    detail := CASE WHEN ok THEN format('bucket %s at %s', current_setting('pgbx.s3_bucket', true), current_setting('pgbx.s3_endpoint', true)) ELSE 'not set: ' || v END;
    fix := CASE WHEN ok THEN NULL ELSE 'set ' || v || ' in postgresql.conf, then SELECT pg_reload_conf()' END;
    RETURN NEXT;

    -- judged from what the worker reported (last_error per database); the file itself is never read here
    name := 'credentials file';
    ok := NOT EXISTS (SELECT 1 FROM pgbx.server_overview s
                      WHERE s.last_error ~* '(credentials_file|access_key_id missing|secret_access_key missing|read [^ ]*credentials)');
    detail := CASE WHEN coalesce(current_setting('pgbx.credentials_file', true), '') = '' THEN 'pgbx.credentials_file is not set'
                   WHEN su THEN 'pgbx.credentials_file = ' || current_setting('pgbx.credentials_file', true)
                   ELSE 'pgbx.credentials_file is set' END
              || CASE WHEN ok THEN '; the worker reported no problem reading it' ELSE '; the worker cannot read it or it lacks access_key_id= / secret_access_key= lines' END;
    fix := CASE WHEN ok THEN NULL ELSE 'make the file readable by the postgres OS user (chmod 600, chown postgres) with access_key_id= and secret_access_key= lines' END;
    RETURN NEXT;

    name := 'database backups';
    SELECT count(*) INTO n FROM pgbx.server_overview;
    bad := NULL; ok := true; detail := NULL;
    FOR o IN SELECT s.database, s.schedule, s.last_backup_at FROM pgbx.server_overview s
             WHERE coalesce(s.state, '') NOT ILIKE 'paused%' ORDER BY s.database LOOP
        lim := pgbx._overdue_after(o.schedule);
        IF o.last_backup_at IS NULL THEN
            ok := false; bad := o.database; detail := format('database %s has no backup yet', o.database); worst_ratio := 1e9;
        ELSIF lim IS NOT NULL AND extract(epoch FROM now() - o.last_backup_at) / extract(epoch FROM lim) > greatest(worst_ratio, 1) THEN
            ok := false; bad := o.database; worst_ratio := extract(epoch FROM now() - o.last_backup_at) / extract(epoch FROM lim);
            detail := format('database %s: newest backup is %s old, overdue after %s (schedule: %s)',
                             o.database, date_trunc('second', now() - o.last_backup_at), lim, o.schedule);
        END IF;
    END LOOP;
    IF n = 0 THEN
        ok := false; detail := 'no database has reported in yet'; fix := 'wait for the scheduler worker (see the "workers" check)';
    ELSIF ok THEN
        detail := format('all %s databases have a backup within their schedule', n); fix := NULL;
    ELSE
        fix := format('connect to %s, check SELECT * FROM pgbx.status(), then SELECT pgbx.backup_now()', bad);
    END IF;
    RETURN NEXT;

    name := 'restore tests';
    SELECT string_agg(s.database || ': ' || s.last_verify, '; ' ORDER BY s.database) INTO v
      FROM pgbx.server_overview s WHERE s.last_verify LIKE 'FAILED%';
    ok := v IS NULL;
    detail := CASE WHEN ok THEN format('no failed restore test (%s of %s databases tested so far)',
                                       (SELECT count(*) FROM pgbx.server_overview s WHERE s.last_verify IS NOT NULL),
                                       (SELECT count(*) FROM pgbx.server_overview))
                   ELSE 'last restore test failed for ' || v END;
    fix := CASE WHEN ok THEN NULL ELSE 'connect to that database, check SELECT * FROM pgbx.status(), then SELECT pgbx.verify_now()' END;
    RETURN NEXT;

    -- the scheduler talks to Postgres through ordinary connections (application_name 'pgbx'), so it is
    -- judged by its heartbeat: it stamps server_overview.seen_at every poll
    name := 'workers';
    ok := EXISTS (SELECT 1 FROM pg_stat_activity s WHERE s.backend_type = 'pgbx scheduler')
          OR coalesce((SELECT max(s.seen_at) FROM pgbx.server_overview s) > now() - interval '10 minutes', false);
    detail := CASE WHEN ok THEN 'the scheduler worker is alive (recent heartbeat)' ELSE 'no recent heartbeat from: pgbx scheduler' END;
    fix := CASE WHEN ok THEN NULL ELSE 'make sure pgbx is in shared_preload_libraries and restart Postgres; a crashed worker restarts within 10 s — check the Postgres log' END;
    RETURN NEXT;

    -- informational only: pgbx itself never needs WAL archiving
    name := 'archive_mode';
    v := coalesce(current_setting('archive_command', true), '');
    ok := true;
    detail := 'archive_mode = ' || current_setting('archive_mode') ||
              CASE WHEN current_setting('archive_mode') <> 'off' AND v IN ('', '(disabled)')
                   THEN ' with no archive_command: archive_mode=on with no archive_command set by pgbx is not needed for pgbx'
                   ELSE '' END;
    fix := CASE WHEN current_setting('archive_mode') <> 'off' AND v IN ('', '(disabled)')
                THEN 'optional: if nothing else uses WAL archiving, set archive_mode = off at the next planned restart' END;
    RETURN NEXT;

    name := 'replication_slots';
    SELECT string_agg(format('%s (%s, %s, holds %s)', r.slot_name, r.slot_type, coalesce(r.wal_status, '?'),
                             coalesce(pg_size_pretty(pg_wal_lsn_diff(pg_current_wal_lsn(), r.restart_lsn)), '?')), ', '),
           count(*)
      INTO v, n FROM pg_replication_slots r WHERE NOT r.active AND r.restart_lsn IS NOT NULL;
    ok := n = 0;
    detail := CASE WHEN ok THEN 'no inactive replication slot is holding WAL' ELSE 'inactive slot(s) pinning WAL: ' || v END
        || format('; max_slot_wal_keep_size = %s, max_wal_size = %s, wal_keep_size = %s',
                  current_setting('max_slot_wal_keep_size'), current_setting('max_wal_size'), current_setting('wal_keep_size'));
    fix := CASE WHEN ok THEN
                CASE WHEN current_setting('max_slot_wal_keep_size') = '-1' AND EXISTS (SELECT 1 FROM pg_replication_slots)
                     THEN 'consider max_slot_wal_keep_size (e.g. ''10GB'') so a dead consumer cannot fill the disk' END
           ELSE '(destructive, needs human approval: a dropped slot cannot be recreated at the same position and its consumer must be rebuilt) drop the slot only if its consumer is gone for good: SELECT pg_drop_replication_slot(''<name>''); '
                || 'and set max_slot_wal_keep_size (e.g. ''10GB'') so a dead consumer cannot fill the disk' END;
    RETURN NEXT;

    -- a running dump holds ACCESS SHARE on every table it reads until it ends: DDL on them waits for it
    name := 'long_running_job';
    lim := make_interval(secs => coalesce((SELECT s.setting::int FROM pg_settings s WHERE s.name = 'pgbx.doctor_long_job'), 3600));
    SELECT string_agg(format('%s in %s for %s (pid %s)', a.application_name, a.datname,
                             date_trunc('second', now() - a.backend_start), a.pid), ', ' ORDER BY a.backend_start)
      INTO v FROM pg_stat_activity a
     WHERE a.application_name IN ('pgbx_dump', 'pgbx_restore', 'pgbx_verify')
       AND lim > interval '0' AND now() - a.backend_start > lim;
    ok := v IS NULL;
    detail := CASE WHEN lim = interval '0' THEN 'check off (pgbx.doctor_long_job = 0)'
                   WHEN ok THEN format('no backup or restore process running longer than %s', lim)
                   ELSE format('running longer than %s: %s', lim, v) END;
    fix := CASE WHEN ok THEN NULL
                ELSE 'a running dump blocks DDL (ALTER TABLE, migrations) on the tables it reads; move the schedule to a quiet hour, '
                     || 'or skip the rows of big tables with pgbx.set_data_scope()' END;
    RETURN NEXT;
END $$;

-- Health checks for the CLI: one row per check, plain-language detail and the fix. Admin database only.
-- Viewers may call it; every text is scrubbed of anything that looks like a key or signature.
CREATE FUNCTION pgbx.doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
LANGUAGE sql STABLE AS $$
    SELECT d.name, d.ok, pgbx._scrub(d.detail), pgbx._scrub(d.fix) FROM pgbx._doctor() d
$$;

-- internal: pg_dump-style pattern ('schema.table', * and ?; bare name = public) -> LIKE pattern
CREATE FUNCTION pgbx._like(p text) RETURNS text LANGUAGE sql IMMUTABLE AS $$
    SELECT replace(replace(
             replace(replace(replace(CASE WHEN position('.' IN p) = 0 THEN 'public.' || p ELSE p END,
                                     '\', '\\'), '_', '\_'), '%', '\%'),
           '*', '%'), '?', '_')
$$;

-- Tables whose ROWS the next backup skips (definitions are always kept). pgbx's own tables never appear.
CREATE FUNCTION pgbx.rowless_tables() RETURNS TABLE (table_name text) LANGUAGE sql STABLE AS $$
    SELECT format('%I.%I', n.nspname, c.relname)
    FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
    CROSS JOIN (SELECT include_data, exclude_data FROM pgbx.config) cfg
    WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'pgbx')
      AND n.nspname NOT LIKE 'pg_toast%'
      AND (   (coalesce(cardinality(cfg.include_data), 0) > 0
               AND NOT EXISTS (SELECT 1 FROM unnest(cfg.include_data) p WHERE n.nspname || '.' || c.relname LIKE pgbx._like(p)))
           OR EXISTS (SELECT 1 FROM unnest(coalesce(cfg.exclude_data, '{}')) p WHERE n.nspname || '.' || c.relname LIKE pgbx._like(p)))
    ORDER BY 1
$$;

-- Which tables keep their ROWS in backups. Table definitions are ALWAYS backed up, so a restore never misses a table.
--   set_data_scope(exclude => ARRAY['public.sessions', 'audit_log_*'])   -- skip rows of these
--   set_data_scope(include => ARRAY['billing.*', 'users'])               -- keep rows of only these
--   set_data_scope()                                                     -- everything (default)
CREATE FUNCTION pgbx.set_data_scope(include text[] DEFAULT NULL, exclude text[] DEFAULT NULL) RETURNS text
LANGUAGE plpgsql AS $$
DECLARE n bigint;
BEGIN
    INSERT INTO pgbx.config DEFAULT VALUES ON CONFLICT (id) DO NOTHING;
    UPDATE pgbx.config SET include_data = nullif(set_data_scope.include, '{}'),
                                  exclude_data = nullif(set_data_scope.exclude, '{}'), updated_at = now();
    PERFORM pgbx._log('config', jsonb_build_object('include_data', set_data_scope.include, 'exclude_data', set_data_scope.exclude));
    SELECT count(*) INTO n FROM pgbx.rowless_tables();
    RETURN CASE WHEN set_data_scope.include IS NULL AND set_data_scope.exclude IS NULL
                THEN 'backups keep every table and every row'
                ELSE format('backups keep every table definition; rows skipped for %s table(s) right now — see pgbx.rowless_tables()', n) END;
END $$;

-- Queue a backup now. Returns the history id; watch it in pgbx.history. While a backup of this database is still
-- queued it returns that one instead (pgbx.coalesce_manual), so calling it five times costs one dump.
CREATE FUNCTION pgbx.backup_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('backup');
END $$;

-- Queue a restore of this database's newest backup taken at or before `at` into a NEW database `into_db`.
-- The live database is never touched; swap names yourself once the restore is verified.
CREATE FUNCTION pgbx.restore(into_db text, at timestamptz DEFAULT now()) RETURNS bigint LANGUAGE sql AS $$
    INSERT INTO pgbx.history (kind, trigger, params)
    VALUES ('restore', 'manual', jsonb_build_object('into_db', into_db, 'at', at)) RETURNING id
$$;
"#,
    name = "schema",
    bootstrap
);

extension_sql!(
    r#"
-- ---- privileges: nothing for PUBLIC; viewer reads; admin manages through SECURITY DEFINER functions ----
REVOKE ALL ON SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL TABLES IN SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL SEQUENCES IN SCHEMA pgbx FROM PUBLIC;
REVOKE ALL ON ALL FUNCTIONS IN SCHEMA pgbx FROM PUBLIC;
GRANT USAGE ON SCHEMA pgbx TO pgbx_viewer;

-- viewer: read-only
GRANT SELECT ON pgbx.config, pgbx.history, pgbx.backups, pgbx.server_overview TO pgbx_viewer;
GRANT EXECUTE ON FUNCTION pgbx.status(), pgbx.overview(), pgbx.to_cron(text),
      pgbx.next_run_epoch(text, double precision), pgbx._require_admin_db(),
      pgbx.rowless_tables(), pgbx._like(text) TO pgbx_viewer;
-- doctor() reads server-wide settings and slots: runs as the extension owner, viewers may call it
ALTER FUNCTION pgbx.doctor() SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.doctor() TO pgbx_viewer;

-- admin: management functions run as the extension owner (so admins never need write access to the tables)
DO $lock$
DECLARE f text;
BEGIN
    FOREACH f IN ARRAY ARRAY[
        'pgbx.configure(text, int, int, bool, text)', 'pgbx.set_schedule(text)',
        'pgbx.set_retention(int, int)', 'pgbx.pause(text)', 'pgbx.resume()',
        'pgbx.backup_now()', 'pgbx.restore(text, timestamptz)', 'pgbx.verify_now()',
        'pgbx.set_verify_schedule(text)', 'pgbx.download_url(bigint, interval)',
        'pgbx.set_data_scope(text[], text[])', 'pgbx.cancel(bigint)']
    LOOP
        EXECUTE format('ALTER FUNCTION %s SECURITY DEFINER SET search_path = pg_catalog, pgbx', f);
        EXECUTE format('GRANT EXECUTE ON FUNCTION %s TO pgbx_admin', f);
    END LOOP;
END $lock$;
-- internals (_presign, _log, _check_days, _queue_manual): superuser only — no grants.
"#,
    name = "lockdown",
    finalize
);

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
