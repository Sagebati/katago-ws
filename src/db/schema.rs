//! Diesel schema for the single `jobs` table. Kept in sync with the
//! `create_job_store` migration.

/// SQL-side marker for the Postgres `job_status` enum. The Rust value type is
/// [`crate::db::status::JobStatus`], mapped via `diesel-derive-enum`.
pub mod sql_types {
    // `QueryId` (beyond `SqlType`) lets this enum be a query-cacheable bind, so a
    // `SELECT`/`COUNT` can `.filter(status.eq(...))` — see `db::jobs_in_state` /
    // `db::count_in_state`. Without it only `UPDATE … WHERE status = …` compiles.
    #[derive(diesel::sql_types::SqlType, diesel::query_builder::QueryId)]
    #[diesel(postgres_type(name = "job_status"))]
    pub struct JobStatus;
}

diesel::table! {
    use diesel::sql_types::*;
    use super::sql_types::JobStatus;

    /// One row per job: the durable system of record AND the realtime read model.
    /// Updated in place as the job moves; `change_seq` (trigger-maintained) is the
    /// monotonic cursor the SSE stream tails.
    jobs (id) {
        id -> Uuid,
        status -> JobStatus,
        result -> Nullable<Jsonb>,
        error -> Nullable<Text>,
        last_error -> Nullable<Text>,
        created_at -> Timestamptz,
        updated_at -> Timestamptz,
        change_seq -> Int8,
    }
}
