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
            RETURN j;
        END IF;
    END IF;
    INSERT INTO pgbx.history (kind, trigger) VALUES (k, 'manual') RETURNING id INTO j;
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
    queued_jobs bigint, location text, running_job bigint, next_job bigint, queue_position int, waiting_reason text
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
        coalesce(nq.params->>'wait_reason', CASE WHEN nq.id IS NOT NULL THEN 'queued; the worker picks it up within pgbx.poll_seconds' END)
    FROM (SELECT NULL::bigint AS id, NULL::jsonb AS params
          UNION ALL (SELECT h.id, h.params FROM pgbx.history h WHERE h.state='queued'
                     ORDER BY (h.params->>'queue_position')::int NULLS LAST, h.id LIMIT 1)
          ORDER BY id NULLS LAST LIMIT 1) nq;
END $$;
REVOKE ALL ON FUNCTION pgbx.status() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgbx.status() TO pgbx_viewer;
