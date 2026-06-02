-- Single job store: one LOGGED `jobs` table is BOTH the durable system of record
-- and the realtime read model. The split into queue + analyses + job_events was
-- collapsed here — a job's row is updated in place as it moves, so there's no
-- separate event log and no union view to reconcile.
--
-- Roles:
--   * the pgmq queue (logged, elsewhere) is the AUTHORITY for in-flight WORK and
--     carries the SGF, so a crash replays unfinished jobs;
--   * this table holds STATE + the durable outcome, and a monotonic `change_seq`
--     cursor that the SSE stream tails.
--
-- Every write stamps `change_seq` and `updated_at` via a trigger (not the app),
-- so no write can forget the cursor and `updated_at` stays authoritative at the DB.

-- UUIDv7 generator. Postgres 16 has no native `uuidv7()` (it arrived in PG 18),
-- so we synthesize one: take a random v4 UUID, overlay its first 48 bits with the
-- current Unix time in milliseconds, then set the version nibble to 7. The result
-- is time-ordered. On PG 18+, drop this and use the built-in `uuidv7()`.
CREATE FUNCTION uuidv7() RETURNS uuid
    LANGUAGE sql VOLATILE AS $$
    SELECT encode(
        set_bit(
            set_bit(
                overlay(
                    uuid_send(gen_random_uuid())
                    PLACING substring(int8send(floor(extract(epoch FROM clock_timestamp()) * 1000)::bigint) FROM 3)
                    FROM 1 FOR 6
                ),
                52, 1
            ),
            53, 1
        ),
        'hex'
    )::uuid;
$$;

-- Lifecycle states as a real enum (Rust `JobStatus` maps to this 1:1).
CREATE TYPE job_status AS ENUM ('queued', 'running', 'done', 'failed');

-- Global monotonic change counter — the realtime stream's cursor. Gapless and
-- clock-independent (unlike a timestamp), unique per write (no ties to force a
-- `>` / `>=` dilemma), and filter-agnostic, so it still works when a per-user
-- stream later filters by user and tails its own position in this one sequence.
CREATE SEQUENCE jobs_change_seq;

CREATE TABLE jobs (
    id          UUID        PRIMARY KEY DEFAULT uuidv7(),
    status      job_status  NOT NULL DEFAULT 'queued',
    result      JSONB,                              -- present iff status = done
    error       TEXT,                               -- present iff status = failed
    last_error  TEXT,                               -- most recent transient attempt error (observability)
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(), -- submit time (row insert)
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(), -- last transition (trigger-maintained)
    change_seq  BIGINT      NOT NULL,               -- stream cursor (trigger-maintained)
    -- result⊕error as a hard guarantee: a terminal row is exactly a success
    -- (result, no error) or a failure (error, no result); a non-terminal row has
    -- neither. (`last_error` is observability, not governed by this.)
    CONSTRAINT jobs_result_xor_error CHECK (
        (status IN ('queued', 'running') AND result IS NULL     AND error IS NULL) OR
        (status = 'done'                 AND result IS NOT NULL  AND error IS NULL) OR
        (status = 'failed'               AND error  IS NOT NULL  AND result IS NULL)
    )
);

-- Tail index for the stream: `WHERE change_seq > :cursor ORDER BY change_seq`.
CREATE INDEX jobs_change_seq_idx ON jobs (change_seq);
-- Newest-first listing for `GET /analyses`.
CREATE INDEX jobs_created_at_idx ON jobs (created_at DESC);

-- Stamp the monotonic cursor and last-change time on every write. In a trigger
-- (not the app) so no code path can forget it and `updated_at` is DB-authoritative.
CREATE FUNCTION jobs_stamp_change() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    NEW.change_seq := nextval('jobs_change_seq');
    NEW.updated_at := now();
    RETURN NEW;
END;
$$;

CREATE TRIGGER jobs_stamp_change
    BEFORE INSERT OR UPDATE ON jobs
    FOR EACH ROW EXECUTE FUNCTION jobs_stamp_change();
