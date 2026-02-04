//! Supabase-Compatible REST API
//!
//! Drop-in compatible REST API that follows Supabase/PostgREST conventions
//! for easy migration from Supabase to HeliosDB-Lite.

pub mod postgrest;
pub mod auth;
pub mod storage;
pub mod realtime;
pub mod routes;

pub use postgrest::*;
pub use routes::*;
