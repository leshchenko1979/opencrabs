//! Query-relevance scoring for message recall.
//!
//! One home for the ranking rule, so the shipped search path and its offline
//! eval harness cannot drift apart again (#911). [`episodic`] scores one row
//! against one query and orders a recency-ordered pool by that score.

pub mod episodic;
