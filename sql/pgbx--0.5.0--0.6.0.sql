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

-- coalesced manual jobs and cancel (ADR 0001 §0); the server-wide queue itself is not in 0.6.0
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
    END IF;
    RAISE EXCEPTION 'pgbx: job % is % (only a queued job can be cancelled)', job_id, h.state;
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
