//! The scenarios, one module each. Every module exports
//! `pub fn scenarios() -> Vec<Box<dyn crate::Scenario>>`.

pub mod chat;
pub mod chat_patterns;
pub mod focusable;
pub mod form;
pub mod layout;
pub mod list;
pub mod nested_cached;
pub mod paths;
pub mod scroll;
pub mod scrollbar;
pub mod settings;
pub mod table;
pub mod workspace;
