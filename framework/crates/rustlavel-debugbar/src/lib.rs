//! A debug bar: what *this* request did, on the page it produced.
//!
//! `rustlavel-telescope` is the other half of this idea and answers a different
//! question. It is a dashboard at its own URL, and you go to it to ask what
//! happened across many requests. A debug bar answers "what did the page I am
//! looking at just do", without leaving the page — which is the question you
//! have while building a screen, and the one where walking to another tab
//! breaks your train of thought.
//!
//! Both read the same instrumentation bus. Neither collects anything the other
//! does not; they differ only in where they put it.
//!
//! # It does not run in production
//!
//! The bar prints SQL, cache keys and timings into the HTML of a page. That is
//! exactly what you want while developing and exactly what you must never ship,
//! so it is off unless the environment says otherwise, and turning it on in
//! production takes a method whose name says what you are doing.

pub mod collector;
pub mod plugin;
pub mod render;

pub use collector::{Collected, Collector, Timing};
pub use plugin::DebugBar;

pub use rustlavel_core::{Error, Result};
