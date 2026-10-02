-- pgbx 0.5.0 -> 0.6.0. The worker runs ALTER EXTENSION pgbx UPDATE in every database and template1 by itself.
-- Objects changed here must match their CREATE in src/lib.rs (unit test worker::t::update_script_matches_install).

-- server-wide queue (ADR 0001 §0): what doctor() needs to see dumps longer than their schedule interval
ALTER TABLE pgbx.server_overview ADD COLUMN interval_secs float8, ADD COLUMN dump_secs float8[];

-- doctor(): long_running_job (ADR 0001 §3), dump_longer_than_interval (§0)
CREATE OR REPLACE FUNCTION pgbx._doctor() RETURNS TABLE (name text, ok bool, detail text, fix text)
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

    -- each of the last 3 dumps ran longer than the schedule interval: slots get skipped (or run back to back)
    name := 'dump_longer_than_interval';
    v := NULL; bad := NULL; fix := NULL;
    FOR o IN SELECT s.database, s.interval_secs, s.dump_secs, (SELECT min(d) FROM unnest(s.dump_secs) d) AS shortest,
                    (SELECT max(d) FROM unnest(s.dump_secs) d) AS longest
               FROM pgbx.server_overview s
              WHERE coalesce(s.state, '') NOT ILIKE 'paused%' AND s.interval_secs > 0 AND cardinality(s.dump_secs) >= 3
              ORDER BY s.database LOOP
        CONTINUE WHEN o.shortest <= o.interval_secs;
        v := concat_ws('; ', v, format('%s: last 3 backups took %s; the schedule runs every %s', o.database,
                 (SELECT string_agg(make_interval(secs => round(d))::text, ', ') FROM unnest(o.dump_secs) d),
                 make_interval(secs => round(o.interval_secs))));
        IF bad IS NULL THEN
            bad := o.database;
            SELECT l.label INTO fix FROM (VALUES ('every 15 minutes', 900), ('every 30 minutes', 1800), ('every 1 hour', 3600),
                    ('every 2 hours', 7200), ('every 3 hours', 10800), ('every 4 hours', 14400), ('every 6 hours', 21600),
                    ('every 12 hours', 43200), ('daily', 86400), ('weekly', 604800)) l(label, secs)
             WHERE l.secs >= 2 * o.longest ORDER BY l.secs LIMIT 1;
        END IF;
    END LOOP;
    ok := v IS NULL;
    detail := CASE WHEN ok THEN 'recent backups of every database finish within its schedule interval' ELSE v END;
    fix := CASE WHEN ok THEN NULL
                ELSE format('connect to %s and give it a longer schedule: SELECT pgbx.configure(schedule => %L);',
                            bad, coalesce(fix, 'weekly')) END;
    RETURN NEXT;

    -- informational: what the time estimates are based on (ADR 0001 §4)
    name := 'capacity';
    ok := true; fix := NULL;
    SELECT format('one core compresses %s/s (%s)%s; disk reads %s; upload %s, download %s; load x%s; %s core(s)',
                  coalesce(pg_size_pretty(c.cpu_bps::bigint), '?'), coalesce(c.cpu_codec, 'not measured'),
                  coalesce(', measured ' || date_trunc('minute', c.measured_at)::text, ''),
                  coalesce(pg_size_pretty(c.disk_bps::bigint) || '/s', 'unknown (track_io_timing off)'),
                  coalesce(pg_size_pretty(c.upload_bps::bigint) || '/s', 'not seen yet'),
                  coalesce(pg_size_pretty(c.download_bps::bigint) || '/s', 'not seen yet'),
                  round(coalesce(c.load_factor, 1)::numeric, 1), coalesce(c.cores::text, '?'))
      INTO detail FROM pgbx.server_capacity c;
    detail := coalesce(detail, 'not measured yet (the worker measures it once a day, pgbx.eta_calibrate)');
    RETURN NEXT;

    -- how good the time estimates were: median |actual - estimate| / actual over each database's recent jobs
    name := 'eta_accuracy';
    SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY s.eta_error), count(*) INTO worst_ratio, n
      FROM pgbx.server_overview s WHERE s.eta_error IS NOT NULL;
    ok := n = 0 OR worst_ratio <= 0.5;
    detail := CASE WHEN n = 0 THEN 'no finished job with an estimate yet'
                   ELSE format('estimates were off by %s %% (median over %s database(s))', round((100 * worst_ratio)::numeric), n) END;
    fix := CASE WHEN ok THEN NULL
                ELSE 'estimates settle after 3 runs of a job; if they stay off, check pgbx.upload_kbps / load, or lower pgbx.eta_samples so they follow recent growth' END;
    RETURN NEXT;
END $$;

-- coalesced manual jobs and cancel (ADR 0001 §0)
ALTER TABLE pgbx.history DROP CONSTRAINT IF EXISTS history_state_check;
ALTER TABLE pgbx.history ADD CONSTRAINT history_state_check
    CHECK (state IN ('queued', 'running', 'done', 'failed', 'expired', 'cancelled'));

-- internal: queue a manual job, or (pgbx.coalesce_manual, default on) return the one of that kind already queued
-- in this database
CREATE OR REPLACE FUNCTION pgbx._queue_manual(k text) RETURNS bigint LANGUAGE plpgsql AS $$
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
            PERFORM pgbx._notice_eta(j);
            RETURN j;
        END IF;
    END IF;
    INSERT INTO pgbx.history (kind, trigger) VALUES (k, 'manual') RETURNING id INTO j;
    PERFORM pgbx._notice_eta(j);
    RETURN j;
END $$;

-- Cancel a queued job of this database: it never starts and ends as 'cancelled'.
CREATE OR REPLACE FUNCTION pgbx.cancel(job_id bigint) RETURNS text LANGUAGE plpgsql AS $$
DECLARE h pgbx.history;
BEGIN
    SELECT * INTO h FROM pgbx.history WHERE id = job_id FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'pgbx: no job % in database %', job_id, current_database();
    ELSIF h.state = 'queued' THEN
        UPDATE pgbx.history SET state = 'cancelled', finished = now(), error = 'cancelled by ' || session_user WHERE id = job_id;
        RETURN format('%s job %s cancelled before it started', h.kind, job_id);
    ELSIF h.state = 'running' THEN
        UPDATE pgbx.history SET params = params || jsonb_build_object('cancel_requested', now(), 'cancelled_by', session_user)
         WHERE id = job_id;
        RETURN format('%s job %s is running: the worker stops it within a few seconds and it ends as cancelled '
                      '(nothing is left in S3)', h.kind, job_id);
    END IF;
    RAISE EXCEPTION 'pgbx: job % is % already (only a queued or running job can be cancelled)', job_id, h.state;
END $$;

CREATE OR REPLACE FUNCTION pgbx.verify_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('verify');
END $$;

CREATE OR REPLACE FUNCTION pgbx.backup_now() RETURNS bigint LANGUAGE plpgsql AS $$
BEGIN
    RETURN pgbx._queue_manual('backup');
END $$;

REVOKE ALL ON FUNCTION pgbx._queue_manual(text), pgbx.cancel(bigint) FROM PUBLIC;
ALTER FUNCTION pgbx.cancel(bigint) SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.cancel(bigint) TO pgbx_admin;
-- CREATE OR REPLACE resets SECURITY DEFINER / search_path: restore them as the install's lockdown sets them
ALTER FUNCTION pgbx.backup_now() SECURITY DEFINER SET search_path = pg_catalog, pgbx;
ALTER FUNCTION pgbx.verify_now() SECURITY DEFINER SET search_path = pg_catalog, pgbx;

-- the server-wide job queue (ADR 0001 §0): pgbx jobs, cancel of a running job, status() says why a job waits
-- The server-wide job queue as the worker sees it, rewritten every poll in the admin database.
CREATE TABLE pgbx.server_queue (
    database     name NOT NULL,
    job_id       bigint NOT NULL,
    kind         text NOT NULL,
    trigger      text NOT NULL,
    state        text NOT NULL,                                -- running | cancelling | queued | deferred
    position     int,                                          -- 1 = starts next; NULL while running or deferred
    slot         int,                                          -- job slot while running; 0 = the restore lane
    requested_at timestamptz,
    started_at   timestamptz,
    detail       text,                                         -- why it waits, or where it runs
    seen_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (database, job_id)
);
GRANT SELECT ON pgbx.server_queue TO pgbx_viewer;

-- status() gains running_job, next_job, queue_position, waiting_reason (new OUT columns: drop and create)
DROP FUNCTION pgbx.status();
CREATE OR REPLACE FUNCTION pgbx.status() RETURNS TABLE (
    database name, state text, schedule text, cron text, next_backup_at timestamptz,
    last_backup_at timestamptz, last_backup_age interval, last_backup_size text, last_backup_key text,
    backups_kept bigint, retention text, data_scope text, verify_schedule text, last_verified_at timestamptz, last_verify_result text,
    last_error text, last_error_at timestamptz, paused_reason text, paused_at timestamptz,
    queued_jobs bigint, location text, running_job bigint, next_job bigint, queue_position int, waiting_reason text,
    job_progress text, job_eta text
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
               coalesce(cfg.path, current_database())),
        (SELECT max(h.id) FROM pgbx.history h WHERE h.state='running'),
        nq.id, (nq.params->>'queue_position')::int,
        coalesce(nq.params->>'wait_reason', CASE WHEN nq.id IS NOT NULL THEN 'queued; the worker picks it up within pgbx.poll_seconds' END),
        (SELECT j.progress FROM pgbx.history h, pgbx.job_eta(h.id) j WHERE h.state='running' ORDER BY h.id DESC LIMIT 1),
        CASE WHEN nq.id IS NOT NULL THEN (SELECT j.progress FROM pgbx.job_eta(nq.id) j)
             WHEN cfg.enabled THEN format('next backup %s, takes ~%s',
                 CASE WHEN last_auto IS NULL THEN 'within a minute (first backup)'
                      ELSE 'at ' || to_char(to_timestamp(pgbx.next_run_epoch(cfg.schedule, extract(epoch FROM last_auto))), 'YYYY-MM-DD HH24:MI') END,
                 (SELECT pgbx._dur(e.est_secs) FROM pgbx._estimate('backup') e)) END
    FROM (SELECT NULL::bigint AS id, NULL::jsonb AS params
          UNION ALL (SELECT h.id, h.params FROM pgbx.history h WHERE h.state='queued'
                     ORDER BY (h.params->>'queue_position')::int NULLS LAST, h.id LIMIT 1)
          ORDER BY id NULLS LAST LIMIT 1) nq;
END $$;
REVOKE ALL ON FUNCTION pgbx.status() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgbx.status() TO pgbx_viewer;

-- time estimates (ADR 0001 §4): job_eta(), a NOTICE on job create, live progress, capacity, doctor accuracy
ALTER TABLE pgbx.server_queue ADD COLUMN eta_start timestamptz, ADD COLUMN eta_finish timestamptz, ADD COLUMN est_bytes bigint,
    ADD COLUMN done_bytes bigint, ADD COLUMN progress text;
ALTER TABLE pgbx.server_overview ADD COLUMN eta_error float8;
-- What this server can do, measured by the worker and copied into every database.
CREATE TABLE pgbx.server_capacity (
    id           int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    cores        int,                                          -- shown only: a dump uses one core
    cpu_bps      float8,                                       -- one core compressing real table pages (pgbx.job_nice)
    cpu_codec    text,                                         -- the pgbx.dump_compression that was measured
    disk_bps     float8,                                       -- reads, from blk_read_time (needs track_io_timing)
    upload_bps   float8,                                       -- median of the last 20 upload parts
    download_bps float8,                                       -- median of recent downloads
    load_factor  float8,                                       -- 1 idle .. 3 saturated: estimates stretch by it
    backup_bps   float8,                                       -- server-wide median speed of recent jobs (dump bytes/s)
    restore_bps  float8,
    verify_bps   float8,
    queue_jobs   int,                                          -- jobs running or waiting, server-wide
    wait_secs    float8,                                       -- estimated wait for a job queued now
    measured_at  timestamptz,                                  -- last cpu probe
    updated_at   timestamptz NOT NULL DEFAULT now()
);
GRANT SELECT ON pgbx.server_capacity TO pgbx_viewer;
CREATE OR REPLACE FUNCTION pgbx._dur(secs float8) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $$
BEGIN
    RETURN CASE WHEN secs IS NULL THEN '?' WHEN secs < 90 THEN round(secs) || ' s' WHEN secs < 5400 THEN round(secs / 60) || ' min'
                WHEN secs < 172800 THEN round((secs / 3600)::numeric, 1) || ' h' ELSE round((secs / 86400)::numeric, 1) || ' d' END;
END $$;
CREATE OR REPLACE FUNCTION pgbx._estimate(k text, OUT est_bytes bigint, OUT est_secs float8, OUT speed_bps float8, OUT confidence text,
                               OUT basis text)
LANGUAGE plpgsql STABLE AS $$
DECLARE cap pgbx.server_capacity; n int := greatest(1, least(50, coalesce(nullif(current_setting('pgbx.eta_samples', true), '')::int, 5)));
        ratio float8; speeds float8[]; s float8; why text; capkb int; srv float8;
BEGIN
    SELECT * INTO cap FROM pgbx.server_capacity;
    -- a backup is the database size times this database's compression ratio (0.3 until known); a restore, its dump
    SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x.r) INTO ratio
      FROM (SELECT h.bytes::float8 / (h.params->>'db_size')::float8 AS r FROM pgbx.history h
             WHERE h.kind = 'backup' AND h.state IN ('done', 'expired') AND h.bytes > 0 AND (h.params->>'db_size')::bigint > 0
             ORDER BY h.id DESC LIMIT n) x;
    IF k = 'backup' THEN
        est_bytes := (pg_database_size(current_database()) * coalesce(ratio, 0.3))::bigint;
    ELSIF k IN ('restore', 'verify') THEN
        SELECT h.bytes INTO est_bytes FROM pgbx.history h WHERE h.kind = 'backup' AND h.state = 'done' AND h.bytes > 0
         ORDER BY h.id DESC LIMIT 1;
    END IF;
    est_bytes := coalesce(est_bytes, 0);
    SELECT array_agg(x.b / x.secs) INTO speeds
      FROM (SELECT h.bytes::float8 AS b, extract(epoch FROM h.finished - h.started)::float8 AS secs FROM pgbx.history h
             WHERE h.kind = k AND h.state IN ('done', 'expired') AND h.bytes > 0 AND h.finished > h.started
             ORDER BY h.id DESC LIMIT n) x;
    srv := CASE k WHEN 'backup' THEN cap.backup_bps WHEN 'restore' THEN cap.restore_bps WHEN 'verify' THEN cap.verify_bps END;
    IF coalesce(cardinality(speeds), 0) >= least(3, n) THEN
        SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY v) INTO s FROM unnest(speeds) v;
        confidence := CASE WHEN (SELECT max(v) / nullif(min(v), 0) FROM unnest(speeds) v) <= 2 THEN 'high' ELSE 'medium' END;
        basis := format('from the last %s %s jobs of this database', cardinality(speeds), k);
    ELSIF srv > 0 THEN
        s := srv; confidence := 'medium'; basis := format('from recent %s jobs on this server', k);
    ELSE
        -- the slowest of the pipes a dump goes through; raw read / compression rates shrink by the ratio
        SELECT p.pipe, p.rate INTO why, s FROM (VALUES
            ('cpu', CASE WHEN k = 'backup' THEN cap.cpu_bps * coalesce(ratio, 0.3) END),
            ('disk', CASE WHEN k = 'backup' THEN cap.disk_bps * coalesce(ratio, 0.3) END),
            ('upload', CASE WHEN k = 'backup' THEN cap.upload_bps END),
            ('download', CASE WHEN k IN ('restore', 'verify') THEN cap.download_bps END)) p(pipe, rate)
         WHERE p.rate > 0 ORDER BY p.rate LIMIT 1;
        IF s IS NULL THEN
            s := 1e6 * greatest(1, coalesce(nullif(current_setting('pgbx.eta_default_mbps', true), '')::int, 20));
            basis := 'nothing measured yet: pgbx.eta_default_mbps';
        ELSE
            basis := format('limited by %s (%s/s)', why, pg_size_pretty(s::bigint));
        END IF;
        confidence := 'low';
    END IF;
    capkb := coalesce(nullif(current_setting(CASE WHEN k = 'backup' THEN 'pgbx.upload_kbps' ELSE 'pgbx.download_kbps' END, true), '')::int, 0);
    IF capkb > 0 AND capkb * 1024.0 < s THEN
        s := capkb * 1024.0;
        basis := format('limited by %s (%s/s)', CASE WHEN k = 'backup' THEN 'pgbx.upload_kbps' ELSE 'pgbx.download_kbps' END,
                        pg_size_pretty(s::bigint));
    END IF;
    IF coalesce(cap.load_factor, 1) > 1.05 THEN
        s := s / cap.load_factor;
        basis := basis || format(', slower: server busy (x%s)', round(cap.load_factor::numeric, 1));
    END IF;
    speed_bps := s;
    est_secs := greatest(1, est_bytes / s);
END $$;
CREATE OR REPLACE FUNCTION pgbx.job_eta(job_id bigint) RETURNS TABLE (queue_position int, eta_start timestamptz, eta_finish timestamptz,
    est_bytes bigint, done_bytes bigint, confidence text, progress text)
LANGUAGE plpgsql STABLE AS $$
DECLARE h pgbx.history; e record; left_s float8; el float8;
BEGIN
    SELECT * INTO h FROM pgbx.history x WHERE x.id = job_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'pgbx: no job % in database %', job_id, current_database();
    ELSIF h.state NOT IN ('queued', 'running') THEN
        RETURN QUERY SELECT NULL::int, h.started, h.finished, h.bytes, h.bytes, NULL::text, h.state;
        RETURN;
    END IF;
    SELECT * INTO e FROM pgbx._estimate(h.kind);
    IF h.state = 'running' THEN
        est_bytes := coalesce((h.params->>'est_bytes')::bigint, e.est_bytes);
        done_bytes := coalesce(h.bytes, 0);
        el := extract(epoch FROM now() - h.started);
        IF done_bytes > 0 AND el > 5 THEN
            left_s := greatest(est_bytes - done_bytes, 0) / (done_bytes / el); -- the job's own throughput so far
            confidence := 'measured';
        ELSE
            left_s := greatest(coalesce((h.params->>'eta_sec')::float8, e.est_secs) - el, 0);
            confidence := e.confidence;
        END IF;
        queue_position := 0; eta_start := h.started; eta_finish := now() + make_interval(secs => left_s);
        progress := format('%s %% · ~%s left', least(99, floor(100.0 * done_bytes / greatest(est_bytes, 1)))::int, pgbx._dur(left_s));
    ELSE
        queue_position := (h.params->>'queue_position')::int;
        eta_start := greatest(now(), coalesce((h.params->>'eta_start')::timestamptz, now()));
        est_bytes := e.est_bytes; done_bytes := 0; confidence := e.confidence;
        eta_finish := eta_start + make_interval(secs => e.est_secs);
        progress := format('queued%s: starts ~%s, takes ~%s', coalesce(', #' || queue_position || ' in line', ''),
                           CASE WHEN eta_start < now() + interval '1 minute' THEN 'now' ELSE to_char(eta_start, 'HH24:MI') END,
                           pgbx._dur(e.est_secs));
    END IF;
    RETURN NEXT;
END $$;
CREATE OR REPLACE FUNCTION pgbx._notice_eta(j bigint) RETURNS void LANGUAGE plpgsql AS $$
DECLARE h pgbx.history; e record; cap pgbx.server_capacity; ahead int; starts timestamptz;
BEGIN
    SELECT * INTO h FROM pgbx.history x WHERE x.id = j;
    SELECT * INTO e FROM pgbx._estimate(h.kind);
    SELECT * INTO cap FROM pgbx.server_capacity;
    ahead := coalesce(cap.queue_jobs, 0);
    starts := now() + make_interval(secs => coalesce(cap.wait_secs, 0));
    RAISE NOTICE '%', format('pgbx: %s job %s queued%s, starts ~%s, takes ~%s (%s, %s confidence, %s)', h.kind, j,
        CASE WHEN ahead > 0 THEN format(' (%s job(s) running or waiting on this server)', ahead) ELSE '' END,
        CASE WHEN starts < now() + interval '1 minute' THEN 'now' ELSE to_char(starts, 'HH24:MI') END,
        pgbx._dur(e.est_secs), pg_size_pretty(e.est_bytes), e.confidence, e.basis);
END $$;
-- restore() becomes plpgsql to raise the NOTICE (same signature)
CREATE OR REPLACE FUNCTION pgbx.restore(into_db text, at timestamptz DEFAULT now()) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE j bigint;
BEGIN
    INSERT INTO pgbx.history (kind, trigger, params)
    VALUES ('restore', 'manual', jsonb_build_object('into_db', restore.into_db, 'at', restore.at)) RETURNING id INTO j;
    PERFORM pgbx._notice_eta(j);
    RETURN j;
END $$;
REVOKE ALL ON FUNCTION pgbx._dur(double precision), pgbx._estimate(text), pgbx.job_eta(bigint), pgbx._notice_eta(bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgbx._dur(double precision) TO pgbx_viewer;
ALTER FUNCTION pgbx.job_eta(bigint) SECURITY DEFINER SET search_path = pg_catalog, pgbx;
GRANT EXECUTE ON FUNCTION pgbx.job_eta(bigint) TO pgbx_viewer;
ALTER FUNCTION pgbx.restore(text, timestamptz) SECURITY DEFINER SET search_path = pg_catalog, pgbx;
